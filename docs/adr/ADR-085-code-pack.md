# ADR-085: Code Pack — Source-Code Ontology and Audit-Finding Vocabulary

**Status**: Accepted\
**Date**: 2026-07-03\
**Authors**: khive maintainers
**Depends on**: ADR-001 (Entity Kind Taxonomy — `entity_type` subtype registration), ADR-002
(Edge Ontology — closed 20 relations (amended through ADR-197), base endpoint contract), ADR-013 (Note Kind Taxonomy —
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
- ADR-002: Edge Ontology — closed 20 relations (amended through ADR-197); base endpoint contract
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
  signal for which language to build a Scanner for next. The regex tier is
  intentionally scope-blind: guarded/type-check-only and function-local imports
  are included. Its edges and any cycles derived from them therefore mean static
  lexical coupling, not proven module-initialization or runtime dependency; L1.5
  carries no scope discriminator from which a consumer could infer otherwise.
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
When a scanner identity correction changes a symbol UUID, manually authored
edges on the old UUID remain on that historical node and require manual
re-linking to a new symbol UUID.

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

An explicit `db` must resolve to an existing regular file before the handler constructs its target
runtime. A pre-created empty dedicated file may initialize and migrate; omitting `db` retains
automatic creation of `<path>/.khive/code-map.db`. A missing or non-file explicit target is refused
as `RuntimeError::InvalidInput` naming the path, without opening or migrating that target. This
preflight is typo protection, not an inode-identity or concurrent-unlink guarantee: filesystem
replacement between validation and SQLite open remains outside this bounded contract. The
production-path exclusion above still applies before this preflight.

The dedicated target uses the ordinary khive graph and text-search substrates. Each entity upsert
in L1/L1.5 is paired with `entity_fts_document` in the map's text store; a successful
`code.ingest` response therefore means generic KG `search` and query-anchored `context` can read
the entities it reports. An FTS write failure fails the ingest and is repaired by an idempotent
retry, rather than returning an unqualified success for a graph whose empty search results would
look authoritative. The report exposes the number of completed document writes as `fts_indexed`.
Interactive reads select the map with a dedicated `[[backends]]` config and `--config`. The
multi-backend safety contract refuses a conflicting concrete `--db` override, while permitting
`:memory:` and normalizing a path that canonically matches the declared `main` backend. The
policy-derived `kkernel code-audit` admin command remains a separate, documented read surface.

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
exists is reported as skipped, never overwritten. "Exists" is
history-preserving here: a soft-deleted row has already consumed its id and
is skipped by both the real path and `--dry-run`; re-ingestion never clears
its `deleted_at` marker or otherwise resurrects it. Thus re-running the same
sweep is a no-op and a `finding`'s curated lifecycle state (`kind_status`)
and deletion state are never reset by re-ingestion. `--dry-run` runs the same
including-tombstones existence checks and reports what would happen without
writing.

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
result. None of them writes to any database, production or map: no
entity, edge, or note row is created, updated, or deleted by any of the
three. E7 describes the narrow filesystem side effect a read-only
WAL-mode open can still have on a target database's own `-wal`/`-shm`
sidecars — a storage-engine artifact of opening the file for read, not a
row-level write this scope statement speaks to.

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
count, no aggregation, and no cyclic component. The same tombstone rule
governs every aggregate by endpoint liveness, not by edge-row liveness
alone: an edge row can be live while one of its endpoint entities is
soft-deleted, and such an edge contributes to no coupling number, no
`edges_by_relation` count, no dead-module count, no aggregation, and no
cyclic component, exactly as a soft-deleted edge row would. Both endpoints
of an edge must be live entities for that edge to participate in anything
these three verbs compute.

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
Rows are ordered by total degree (fan_in + fan_out) descending; ties break
by entity id ascending at `level="module"`, and by project name ascending
at `level="project"` (see the project row eligibility rule below) — a
total order at either level, so two identical calls return rows in the
same sequence (E9). The result is bounded by `top_n`
(integer, minimum 1, maximum 500, default 50 — validation contract in E8) rather than offset-paginated: degree ranking cannot be
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

A project is eligible to appear as a row at `level="project"` only if it
contains at least one live (non-soft-deleted) module; a project with no
live modules — none ever ingested, or all since soft-deleted — is excluded
from the result entirely rather than reported with zero degrees. A project
that does have live modules, but no qualifying `depends_on` relationship to
any other project, still appears, with `fan_in=0` and `fan_out=0`, exactly
as the intra-project-only case above illustrates. Ties in total degree at
`level="project"` order deterministically by project name ascending,
because a project's entity id is not the identifier callers reason about
the way a module's declared name is.

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
  `min(top_n, available rows)`, `top_n` (integer, minimum 1, maximum 100,
  default 10 — validation contract in E8), the
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
module entity ids and names and module-to-module `depends_on` pairs, into
memory. `code.health` declares no output that needs project-to-module
containment (`coupling_outliers` is always module-level, per this
section), so the projection carries no `project contains module` pairs.
Neither step materializes
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

A per-database guard admits at most one running analysis call at a time,
and covers all three analysis verbs alike: `code.coupling`, `code.health`,
and `code.cycles` each acquire the same guard before starting work, and a
second call against a database whose guard is already held fails
immediately with a busy error naming the database, rather than queuing
behind the first. The guard is keyed on the target database's main file
(device, inode) identity, read off the opened descriptor, not on the `db`
pathname the caller supplied: two calls that name the same underlying
file through different paths, a hard link or any other alias, resolve to
one guard slot and contend with each other exactly as two calls against
the identical path would. This is deliberate admission control, not a
`code.health`/`code.cycles`-only cost-sharing detail: `code.coupling` at
`level="project"` still aggregates over every module-to-module edge in
the database to compute its degree totals, so a caller who fans out
repeated `code.coupling` scans against the same database needs the same
fail-fast contention signal `code.health` and `code.cycles` already give.
`top_n` bounds only how many aggregated rows a `code.coupling` call
serializes in its response, never how much of the database the
aggregation itself must scan, which is why admission control lives at
the guard rather than at `top_n`. The MCP `request` surface can dispatch
up to 100 ops concurrently in one batch, and a caller who fans out
several analysis calls at once against the same database needs to see
that contention immediately rather than have it silently absorbed by a
queue. The guard releases as soon as the call it is holding for returns,
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
response. `limit` (integer, minimum 1, maximum 100, default 20 —
validation contract in E8) and `max_members` (integer, minimum 1,
maximum 1000, default 100 — validation contract in E8) bound only the
response's size, not detection: `limit` caps the
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

**Superseded in part by Amendment 12:** E7's thin VFS wrapper over the platform default VFS and its allowance for steady-state target WAL/SHM side effects are replaced by the native guarded VFS and rollback DELETE posture.

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
immutability guarantee, verified by E9's acceptance properties, applies to
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
5. Any file the connection may create or write on the target's behalf,
   its `-wal`, `-shm`, or `-journal` companion, is rejected if it already
   exists with a hard-link count greater than one, regardless of what
   that link names. This is a distinct check from steps 3-4's
   production-identity match: a sidecar hard-linked to some file that has
   nothing to do with the production database passes every identity
   comparison above, since it is not the production file, yet a
   read-only WAL-mode open still writes through that sidecar as an
   ordinary consequence of being opened (the same side effect the
   paragraph above this list describes), and a hard-linked sidecar would
   carry that write to whatever else the link names. The rule applies
   only to files the connection may create or write, the target's own
   `-wal`, `-shm`, and `-journal` companions, never to the main database
   file itself, which every one of these verbs opens strictly read-only;
   production-identity matching (steps 3-4) remains the sole check
   governing the main file and is unchanged by this rule. The link-count
   check runs at the same open-time moment as step 4, against the same
   opened descriptor.

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

### E8: Numeric parameter validation

Every bounded numeric parameter accepted by the three analysis verbs is a
non-negative integer with an explicit closed range; there is no clamping
to a bound and no silent coercion of an out-of-range or non-integer
value. A JSON value that is not an integer (a float such as `50.5`, or a
numeral encoded as a string), or that is below the stated minimum or
above the stated maximum, produces a per-op validation error naming the
parameter and its valid range, before any database is opened.

| Verb            | Parameter     | Minimum | Maximum | Default |
| --------------- | ------------- | ------- | ------- | ------- |
| `code.coupling` | `top_n`       | 1       | 500     | 50      |
| `code.health`   | `top_n`       | 1       | 100     | 10      |
| `code.cycles`   | `limit`       | 1       | 100     | 20      |
| `code.cycles`   | `max_members` | 1       | 1000    | 100     |

- `code.coupling`'s `top_n` bounds the coupling result to at most 500
  ordered rows, defaulting to 50 when omitted (E2).
- `code.health`'s `top_n` bounds `coupling_outliers` to at most 100 rows,
  defaulting to 10 when omitted (E3).
- `code.cycles`'s `limit` bounds the number of serialized cyclic
  components to at most 100, defaulting to 20 when omitted (E4).
- `code.cycles`'s `max_members` bounds a single component's serialized
  member list to at most 1000, defaulting to 100 when omitted (E4).

### E9: Acceptance

An implementation of this amendment is acceptance-tested against eighteen
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
12. **Busy error on concurrent analysis, including aliased paths.** A call
    to any of `code.coupling`, `code.health`, or `code.cycles` issued
    against a `db` while another call to any of the three against that
    same underlying database is still in flight fails immediately with a
    busy error naming the database, rather than blocking until the first
    call finishes. This holds even when the two calls spell `db` as two
    different paths that resolve to the same (device, inode) identity, a
    hard link being the concrete case: both calls share one guard slot
    (E3, E4). A concurrent call against a genuinely _different_ database
    is unaffected.
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
    `code.coupling`, a call with a larger `top_n` returns, as a prefix,
    exactly the ordered rows a call with a smaller `top_n` returned over
    the same rows, up to the number of rows actually available, verifying
    the total order defined in E2 together with its min(requested,
    available) bound. The prefix relation is non-strict: when the number
    of rows available is at or below the smaller `top_n`, the two calls
    return the identical full set of available rows, and that equality
    satisfies the property without requiring the larger call to surface
    additional rows that do not exist. A fixture with fewer modules than
    the smaller `top_n` returns every available row from both calls, with
    no invented rows and no error.
15. **Truncated giant component.** `code.cycles` run against a fixture whose
    cyclic component exceeds `max_members` returns that component with
    `truncated: true`, a member list capped at `max_members`, and a
    `member_count` equal to the component's true, untruncated size.
16. **Numeric parameter validation.** Each of `code.coupling`'s and
    `code.health`'s `top_n`, and `code.cycles`'s `limit` and
    `max_members`, called with a value that is not an integer, or that is
    below its stated minimum or above its stated maximum, is rejected
    with a per-op validation error naming the parameter and its valid
    range (E8); the call is never clamped to the nearest bound and never
    silently coerced.
17. **Hard-linked sidecar rejected regardless of what it aliases.** A
    fixture whose target database's `-wal` or `-shm` sidecar is
    hard-linked to some file that is not the production database, or any
    part of it, is still rejected at open, because the link-count check
    (E7) rejects any writable companion with more than one hard link
    independent of what that link names; the main database file itself,
    opened strictly read-only, is unaffected by this check and continues
    to be governed solely by production-identity matching (property 6).
18. **Project row eligibility at `level="project"`.** A fixture project
    with no live (non-soft-deleted) modules is absent from the
    `level="project"` result entirely, never present with zero degrees; a
    fixture project with live modules but no qualifying cross-project
    `depends_on` edges appears in the result with `fan_in=0` and
    `fan_out=0` (E2); and two projects tied on total degree are ordered
    by project name ascending.

### E10: Interface note — verb count delta

The MCP verb table in ADR-023 and the verb counts in `AGENTS.md` and this
repository's `CLAUDE.md` change when the implementation PR for this
amendment lands, not in this docs-only PR. The expected delta is **+3**
verbs (`code.coupling`, `code.health`, `code.cycles`) added to the pack's
existing one (`code.ingest`), against the count current as of Amendment 2's
acceptance (79 verbs on the default pack set) — the implementation PR
should cite the then-current count at merge time, since intervening PRs may
have changed it.

## Amendment 5 (2026-08-01): dependency, coverage, and source provenance

The shipped L1/L1.5 map made positive edges queryable but left three negative
results ambiguous: ecosystem-specific manifest section names were not a
portable production/development filter, a module with no dependency edge did
not say whether its imports were scanned successfully, and a module could not
be joined to path-addressed git, CI, or editor data. This amendment makes those
provenance dimensions explicit without adding a relation, entity kind, verb,
or storage column. The fields live in the existing entity `properties` and
edge `metadata` JSON objects.

### F1: `depends_on` edges carry normalized scopes

Every L1/L1.5 `depends_on` edge carries `metadata.dependency_scopes`, a sorted,
deduplicated array whose closed values are `normal`, `dev`, and `build`.
`metadata.dependency_kinds` remains the evidence vocabulary emitted by the
source ecosystem (`dependencies`, `dev-dependencies`, `devDependencies`,
`import`, and so on); consumers use `dependency_scopes`, not those producer
tokens, for production-graph policy.

The normalization is:

| Source declaration                                             | Scope          |
| -------------------------------------------------------------- | -------------- |
| Cargo `[dependencies]`                                         | `normal`       |
| Cargo `[dev-dependencies]`                                     | `dev`          |
| Cargo `[build-dependencies]`                                   | `build`        |
| Python `[project].dependencies` and optional dependency groups | `normal`       |
| npm `dependencies`, `peerDependencies`, `optionalDependencies` | `normal`       |
| npm `devDependencies`                                          | `dev`          |
| L1.5 module-to-module import                                   | `build`        |
| L1.5 project import with a matching manifest declaration       | declared scope |
| L1.5 undeclared project import                                 | `build`        |

The L1.5 module default makes D3's existing compile-time guidance concrete.
That `build` policy scope does not override B3's scope-blind scanner caveat:
it classifies the edge for filtering but does not prove that the import runs at
module initialization or at runtime.
For project imports, the governing manifest is authoritative when it declares
the imported project: an import of a dev-only dependency remains dev-scoped
instead of fabricating a production back-edge. An undeclared project import
falls back to `build`. Rust's identifier spelling (`some_crate`) resolves to
the declared Cargo dependency spelling (`some-crate`) before target identity
and scope are recorded. Because the edge table has one row per `(namespace,
source, target, relation)`, an edge may carry more than one scope. An edge is
development-only exactly when its scope set is `{dev}`; `normal` or `build` in
the set makes it part of the production dependency graph. Re-ingest upgrades
unresolved references and edge metadata to this normalized contract while
preserving the raw evidence tokens.

### F2: module scan coverage and containment ownership are explicit

Every module produced by a completed L1.5 pass carries:

- `import_scan_status`: `scanned` when every observed non-skipped import
  resolved, `partially_resolved` when at least one did not, and `unscanned`
  while no completed import scan result exists;
- `import_specifier_count`: the number of non-skipped import specifiers the
  scanner observed; and
- `unresolved_import_count`: how many remained unresolved after the
  synchronous B6 re-resolve pass.

Thus `scanned` plus a zero specifier count is a trustworthy negative result,
distinct from `unscanned`; a module may have zero incident `depends_on` edges
without becoming indistinguishable from missing coverage. A successful ingest
does not leave a module from that pass at `unscanned`. Re-ingest recomputes the
status, so a previously partial module becomes `scanned` when its target lands.

`properties.source_project` is the canonical ownership field on every module.
The containing project carries the same field, so either endpoint of a live
`project contains module` edge is sufficient to aggregate modules by project.
Future L2 containment nodes and symbols inherit the same ownership rule.

### F3: modules carry path and revision identity

Every module produced by L1.5 carries:

- `properties.source_path`: a `/`-separated source-file path relative to the
  enclosing git repository root, or relative to the ingested folder when the
  canonical repository-relative path is unavailable (including the no-git-
  repository case); and
- `properties.source_revision`: the repository's `HEAD` object id observed at
  ingest, or the explicit sentinel `unversioned` when no committed revision is
  available.

The scanner reads the working-tree bytes, so `content_hash` remains the
authority for those bytes when the tree is dirty; `source_revision` identifies
the checked-out revision around that observation rather than claiming the file
matches its committed blob. Rust package roots that contain both `src/lib.rs`
and `src/main.rs` use module paths `crate` and `crate::main`, respectively, so
the two physical modules do not collapse onto B4's same module UUID. These
fields are properties, not additions to B4's UUID tuple: re-ingest updates
path/revision/content provenance on the stable semantic module entity instead
of accumulating one entity per commit. Future L2 symbol entities copy the
declaring module's `source_path` and `source_revision`.

Migration boundary: databases ingested before this correction may retain a
stale `crate` module row (and its edges) for binary roots. Correcting those
databases requires a fresh-namespace re-ingest or manual cleanup. The
pre-correction state was itself incorrect because two distinct modules shared
one identity.

### F4: Acceptance

1. A two-project fixture with a normal dependency in one direction and a
   dev-only reciprocal dependency has both edges in the history-preserving
   map, but its production-scope projection has no cycle.
2. A module with no imports reports `scanned` and zero counts; a module with a
   missing target reports `partially_resolved`, then reports `scanned` after
   the target is added and the map is re-ingested.
3. Querying by `(source_project, source_path)` identifies exactly one module in
   the fixture, and its `source_revision` advances when repository `HEAD`
   advances.
4. Both endpoints of every fixture `project contains module` edge expose the
   same non-empty `source_project` value.

## Amendment 6 (2026-08-09): repository-scoped findings identity v2

Amendment 1 A1's tolerated free-form posture remains in force for producer content fields. The
following fields are instead the structural identity envelope and are now governed as non-blank
strings: `audit.date`, `audit.scope`, `audit.repo`, `audit.branch`, `audit.commit`,
`audit.standards_file`, and `findings[].id`. This is the future amendment A1 required before stricter
ingest validation; it is justified because blank identity material collapses unrelated producer
runs and makes provenance unqueryable. Validation preserves the original non-blank string bytes.

The project UUID remains the v1 tuple over namespace, repository, and scope. Finding identity moves
to schema version 2 and adds both `audit.repo` and the computed project UUID to the recursively
canonicalized content tuple. Observation time remains excluded. Consequently:

- the same input in the same namespace/repository/project scope remains idempotent;
- equal producer IDs/content/source runs in different repositories produce different finding notes
  and annotation edges; and
- substantive finding content changes still produce new notes rather than overwriting curated
  history.

### V1 compatibility and migration decision

V1 finding UUIDs omitted repository/project scope and may already denote evidence from multiple
repositories. The runtime must not guess their ownership or rewrite/merge them automatically. New
ingest creates the correctly scoped v2 row and leaves every v1 note and lifecycle state immutable.
Each v2 note records `identity_schema_version=2`, `repo`, `project_id`, and `legacy_id_v1`, where the
last field is the exact UUID the same input would have produced under v1. An operator or future
migration may join that witness to the v1 note and the current annotation target, then explicitly
merge only after provenance is unambiguous. This one-time coexistence is preferred to silently
attaching a colliding legacy finding to the wrong project.

Acceptance:

1. Every governed identity string rejects whitespace-only input before record construction.
2. Equal findings under two `audit.repo` values have disjoint project, note, and edge UUIDs.
3. Repeated same-repository input and changes only to `observed_at` retain stable v2 UUIDs.
4. A v2 note exposes its schema version, repository, project UUID, and parseable deterministic v1
   UUID witness; ingest does not mutate or claim an existing v1 row.

## Amendment 7 (2026-08-11): strict ingest arguments and observed languages

`code.ingest` now validates its complete public argument object before any filesystem or database
access. The closed argument set is `path`, `db`, `languages`, and `tiers`; deserialization rejects
every unknown field by name and reports the accepted field names. This brings the code-pack tranche
of the public verb surface into the same typo-detecting posture as the typed KG, GTD, memory, comm,
knowledge, brain, schedule, and session handlers. Git- and blob-pack argument strictness remains a
separate implementation tranche.

The request's `languages` value remains an allow-list: omission permits every supported language,
while an explicit array restricts discovery. The success report's `languages` field no longer
echoes that allow-list. It is the sorted, deduplicated set actually observed by at least one selected
tier: a discovered manifest counts for L1/L1.5, a discovered source file counts for L1.5, and an
accepted Rust source file counts for L2. A language with no accepted manifest or source file is
absent, and selecting no tiers reports an empty set.

Acceptance:

1. A valid call carrying an unknown argument fails before its target database is created, and the
   error names both the unknown argument and the four accepted fields.
2. An omitted language filter over a Rust-only fixture reports exactly `["rust"]`, never the full
   supported-language allow-list.

## Amendment 8 (2026-08-30): concurrent map-ingest rebasing

Separate `code.ingest` calls may target the same map database concurrently. Each handler opens a
target runtime for the requested database, and parallel MCP dispatch does not serialize those
independent read phases. Deterministic UUIDs prevent duplicate logical rows, but they do not make a
full-row read followed by an unconditional upsert safe: the later upsert can erase properties or
edge evidence added by the other call.

Every code-map entity and edge read-modify-write now uses a bounded fresh-read rebase. The caller
expresses a semantic delta (for example, add one unresolved specifier, merge one dependency kind,
or refresh only the fields owned by a sweep). The mutation reads the current live or tombstoned
row, reapplies that delta, and attempts either conditional insert for an absent row or guarded
replacement for an existing row. A refused insert or replacement discards the attempted full row,
then reads and reapplies the delta again; no stale serialized row is retried. The retry bound is 16
row-level attempts and does not rerun file discovery, parsing, or an ingest request.

Each successful replacement advances `updated_at` strictly beyond the revision it observed, even
when two sweeps supply the same or an older wall-clock timestamp. Entity secret-gate checks retain
their existing per-write behavior. FTS indexing and report counters run only after one row write
wins, so a compare-and-swap refusal is not reported as an additional ingest effect. Existing
authorization, deterministic identities, deletion/revival policy, event behavior, schema, and
public success/error wire shapes are unchanged.

Acceptance:

1. Two separate runtimes are forced to read the same entity revision and concurrently add
   different unresolved specifiers; the final row contains both additions and each caller reports
   exactly one row/FTS effect.
2. Two separate runtimes are forced to read the same dependency-edge revision and concurrently add
   different evidence kinds; the final metadata contains both additions and its revision advances.
3. Conditional entity insertion and edge insertion (including a natural-key collision) leave the
   first inserted row untouched when a competing insert loses.

## Amendment 9 (2026-09-25): explicit map targets and the complete production deny set

**Status**: Accepted (2026-09-25).

### Context

Two passages of this record disagree about whether `code.ingest` may create an explicit target, and
the check that enforces "dedicated map databases only" is narrower than that rule.

- B7, second paragraph (added with #3056, which closed #1793): "An explicit `db` must resolve to an
  existing regular file before the handler constructs its target runtime. A pre-created empty
  dedicated file may initialize and migrate; omitting `db` retains automatic creation of
  `<path>/.khive/code-map.db`."
- E7, the paragraph after its numbered sequence (written before #3056): "The existence check (step 2)
  is analysis-only: `code.ingest` remains create-capable and never requires its target to
  pre-exist".

The code follows B7. `resolve_target_db` in `crates/khive-pack-code/src/db_target.rs` refuses an
explicit target that is not an existing regular file, and `crates/khive-pack-code/src/handlers.rs`
constructs the target runtime only after it passes. The only create path left is the omitted-`db`
default. The repository build in `crates/kkernel/src/repo.rs` depends on this: it creates the empty
map file itself (`File::create_new`) immediately before calling `code.ingest` with an explicit `db`.

B7 says `db` "selects among dedicated map databases only". The check behind it is a deny list of two
entries: `resolve_target_db` refuses the default production anchor (`resolve_db_anchor(None)`) and
the calling runtime's configured database (its `db_path`, or `KHIVE_DB` when that is unresolved),
compared by normalized path. Every other existing regular file is accepted. That includes the events
database a file-backed deployment keeps beside its main database
(`khive_runtime::events_split::events_db_path_beside`, `<main>.events.db`, wired in
`crates/khive-mcp/src/serve.rs`) and the file of any other declared `[[backends]]` entry. An accepted
target is opened with the general runtime constructor, which runs main-store migrations on it and
writes map rows into it (#3298).

E7 names the set its identity checks compare against as "the production database's own main file,
plus whichever of its `-journal`, `-wal`, and `-shm` companions presently exist". Neither the events
database nor declared backends are in it. At this revision the analysis verbs of Amendment 4 and
E7's open-time wrapper are not implemented (`CodePack::dispatch` in
`crates/khive-pack-code/src/pack.rs` routes only `code.ingest`), so the target preflight is the
fence in force; #1855 tracks open-time identity enforcement.

### Decision

1. **An explicit target must exist; only the default is created.** B7's preflight governs
   `code.ingest`. An explicit `db` must name an existing regular file, and an empty file initializes
   and migrates. Omitting `db` is the only way `code.ingest` creates a database, at
   `<path>/.khive/code-map.db`. E7's sentence that `code.ingest` "remains create-capable and never
   requires its target to pre-exist" is superseded for explicit targets and holds only for that
   default. The rest of E7's paragraph stands: `code.ingest` opens its target through the general,
   create-capable constructor for its own writes, and only the analysis verbs are held to the
   read-only constructor class.

2. **The deny set is every store the process knows as production.** A `code.ingest` target, explicit
   or default, and an analysis verb's `db` are refused before any open when they identify a member
   of this set:
   - the default production database anchor;
   - the calling runtime's configured database, or `KHIVE_DB` when the configured path is
     unresolved;
   - the file of every `[[backends]]` entry in the process's loaded configuration;
   - the events database beside each of the above (`events_db_path_beside`), whether or not the
     events split is enabled in this process, because a file left by an earlier run is still that
     store's event plane;
   - the `-journal`, `-wal` and `-shm` companions of every member.

   This set replaces E7's "production identity set" wherever E7 uses that term.

3. **Membership is decided by file identity.** When the target and a member both exist, the target's
   (device, inode) is compared with the member's, so a hard link under an unrelated name is caught as
   well as a symlink or a relative spelling. When either does not exist yet, the existing
   normalized-path comparison applies. The refusal names the member it matched and leaves that file's
   bytes unchanged.

4. **No map marker.** The target rule stays a deny rule over known stores. This amendment neither
   requires nor writes a marker that identifies a database as a code map.

Acceptance, stated before any implementation runs:

1. An explicit `db` naming a missing path is refused and creates nothing; an explicit `db` naming an
   existing empty file ingests (the existing B7 arms, unchanged).
2. An explicit `db` naming the events database beside the runtime's main database is refused, the
   error names that file, and its size and modification time are unchanged afterwards.
3. An explicit `db` naming the file of a declared non-main backend is refused in the same way.
4. A hard link to the runtime's main database under an unrelated name is refused. Control: a byte
   copy of the same file at a new inode is accepted, so the arm tests identity rather than content.
5. A populated map written by an earlier `code.ingest` is still accepted as an explicit target.

### Alternatives considered

- **Accept only databases carrying a code-map marker** (the rule #3298 suggests). A marker row
  written when a map is created, and a refusal of any file without one. It breaks three things this
  record or its callers rely on. B7's empty-file rule: an empty file carries no marker, so it needs a
  carve-out that stamps on first use. The repository build, which pre-creates an empty map. And every
  populated map written before the marker existed: `CodePack::schema_plan` declares no statements and
  the handler opens a map with the general constructor over the `kg` and `code` packs, so a map has
  no table or column that another khive database lacks and cannot be recognised after the fact.
  Admitting those maps needs an adoption rule, and "stamp an unmarked file that passes the deny set"
  is the current rule under another name. What a marker adds is protection against khive databases
  this process does not know about, which is the residual named under Consequences; it can be taken
  up as its own amendment.
- **Write a marker now and enforce it later.** It adds a write and a stored field whose only reader
  would be a future amendment, and nothing checks it in the meantime.
- **Keep E7's create-capable sentence and remove B7's preflight.** It reinstates #1793: a typo in
  `db` creates and migrates a fresh database at the wrong path. The repository build and the target
  preflight tests already depend on the refusal.

### Consequences

- The disagreement between B7 and E7 is resolved in B7's favour, which is what the code does.
- `resolve_target_db` needs every declared backend file and events database path. The code pack's
  handler holds only its own runtime configuration (`RuntimeConfig::db_path` and `events_split`);
  declared backends live in the loaded engine configuration, so the host has to pass that list to the
  handler.
- A process that serves a map as one of its own declared backends can no longer ingest into it; the
  ingest runs from a process that does not serve that map. B7's documented read path, a dedicated
  configuration whose `main` is the map, is already refused as an ingest target by the existing
  runtime-database rule.
- Residual, stated so it is not read as closed: a khive database this process does not know about,
  such as another deployment's main database on the same host, is still accepted as an explicit
  target. The preflight protects against typos and known stores, as B7 already says of it; it is not
  an allow-list.

Refs: #3298, #1855, #1793, #3056.

## Amendment 10 (2026-09-28): bounded source ingest execution

**Status**: Accepted (2026-09-28).

### Context

L1 manifest discovery, L1.5 source scanning, and L2 source reads previously mixed recursive
filesystem work with the async ingest executor. Reopening the governing manifest for every source
also multiplied parsing work in large packages. The reading surfaces are precise: L1/L1.5 read
`Cargo.toml` for Rust, `pyproject.toml` for Python, and `package.json` for TypeScript; L1.5 reads
`.rs`, `.py`, and `.ts` respectively (not `.tsx`); L2 reads only `.rs` and its governing
`Cargo.toml`. The same 2 MiB source ceiling was already used by the L2 scanner on main (source_ingest.rs:51).

An offline regular-file census of three available local checkouts, pruning the walk's hidden and
build directories, found no candidate above 2 MiB. On khive-oss `78ad6641`, the candidate counts
were 52 `Cargo.toml`, 3 `pyproject.toml`, 9 `package.json`, and 1,028 `.rs`, 128 `.py`, 115 `.ts`
(maximum source: 743,833 bytes). On the TypeScript-heavy ARW checkout `6c314a14`, 65
`package.json` and 798 `.ts` candidates had a 125,771-byte maximum `.ts` file. On the
Python-heavy lionagi-oss checkout `b9175445`, one `pyproject.toml` and 1,586 `.py` candidates
had a 272,428-byte maximum `.py` file. This is local impact evidence, not a guarantee for other
repositories.

### Decision

`code.ingest` performs recursive discovery, file reads, and source parsing on blocking workers.
Graph and FTS writes remain on the async side. A sweep parses each governing manifest at most once
per tier and resolves source ownership against that sweep's manifest snapshot, so a repository with
many files in one package does not repeatedly read and parse the same manifest. L2 discovers
manifests from the canonical source paths it actually walked, including files reached through
symlinks that remain inside the ingest root.

The L1/L1.5 manifest and source readers require regular files and cap each input at 2 MiB. They
check metadata on the opened handle and read at most 2 MiB plus one byte from that same handle,
so a file growing during the read cannot allocate without bound. An oversized manifest or L1.5 source
is skipped with a warning; healthy siblings continue. The existing L2 refusal path retains its
per-file parse-failure record and source fingerprint. Oversized manifests and L1.5 sources increment the existing
`manifest_files_refused` and `source_files_refused` counters respectively. L2 retains its
`symbol_parse_failures` counter and per-file warning. No report field or default wire shape changes.

Every manifest and L1.5/L2 source read checks the **opened file descriptor's** resolved path
against the canonical ingest root before reading bytes. A symlink planted after traversal or
metadata preflight cannot redirect a read outside the requested tree. Refused opened manifest and
L1.5 source paths increment their existing refusal counters and emit path-specific warnings;
L2 source refusals follow its established per-file reporting. A dangling or looping symlink is
skipped with a warning, and the source walk records its existing dropped-path count. L2 also deduplicates canonical directory visits, so directory symlink cycles
terminate. This source-file boundary does not change the target-database VFS fence decided in E7
and tracked by #1855.

### Alternatives and residuals

Refusing an entire ingest when one source or manifest exceeds 2 MiB would make a healthy sibling
unavailable and turn a single generated file into a repository-wide failure. This amendment
instead skips that input, reports it, and lets the next sweep include it if it becomes eligible.
An unbounded read would preserve coverage but lets one input dominate worker memory and parsing
time. Checking only the pathname discovered by the walk was rejected because a concurrent writer
can replace it with an external symlink before open (#3449); the opened-descriptor check closes
that particular gap. A file may still be edited while its already-accepted descriptor is read:
the byte ceiling remains enforced, but one sweep is not a transactional snapshot of all files.
The parsed manifest index likewise reflects the manifests observed during that sweep, not edits
made afterward; a later sweep reconciles ordinary edits. Pinning Git blobs or locking the whole
tree would be a different ingest contract and is deferred.

Acceptance:

1. A file exactly at the byte ceiling is readable; one byte over is refused before parsing.
2. An oversized manifest and L1.5 source increment the existing refusal counters, while a small
   sibling still ingests.
3. L2-only ingest retains its oversized Rust parse-failure reporting.
4. Source walks and reads yield the async executor; governing manifest resolution uses one parsed
   snapshot per sweep rather than reopening manifests per file.
5. A source or manifest swapped to an external symlink after preflight is rejected by the opened
   descriptor check. Existing source-walk tests retain canonical excluded-tree and symlink-cycle
   boundaries. This is the source boundary from #3449, separate from #1855 and the E7
   target-database VFS fence.

Refs: #3299, #3292, #3449, #1855.

## Amendment 11 (2026-09-28): State the Code-Map Database Fence That Ships

**Status**: Accepted (2026-09-28)\
**Amends**: ADR-085 Amendment 4 E7 and Amendment 9 A9\
**Tracking**: #1855; opened-handle proof #3552

## Context

E7 describes a thin VFS wrapper that rejects a target from the identity of the file handle SQLite actually opens, including the main database and its journal, WAL, and shared-memory companions. A9 expands the protected set to the production anchor, runtime database, all declared backends, the events database beside each, and their companions. The current `code.ingest` route implements a narrower, useful fence: it rejects the known production paths and compares existing files' path-level identities before constructing the target runtime. The three read-only analysis verbs named in E7 are not shipped; only `code.ingest` is dispatched today.

A delegating wrapper around SQLite's default VFS cannot by itself deliver E7's opened-handle proof. The default VFS opens `-shm` within the main file's `xShmMap`, outside wrapper `xOpen`; its Unix file handle is opaque through the public VFS API, and the Windows native path can follow a reparse point before the wrapper sees the handle. The prior “thin VFS wrapper” design is therefore not an implemented cross-platform boundary.

## Decision

1. **Current production fence.** On the shipped `code.ingest` path, explicit `db` syntax is restricted to plain absolute filesystem paths and an existing regular file; only the omitted-`db` default is create-capable. The target is compared against A9's complete known-production set by normalized path and, for existing files, by path-level file identity (`dev`/`ino` on Unix and the platform file-identity comparison on Windows). This is a **courtesy preflight**, before ordinary SQLite open, and remains useful for a direct production path or hard link. It is not proof about the descriptor SQLite ultimately uses. SQLite's `SQLITE_OPEN_NOFOLLOW` behavior, where the selected native VFS and platform honor it, and native no-follow handling on Unix are additional symlink hardening only; neither substitutes for identity checks on the opened main and companion handles. This amendment makes no claim that the code-pack constructor already requests `SQLITE_OPEN_NOFOLLOW` on every SQLite open.

2. **Final-component symlink refusal in the preflight.** The courtesy preflight rejects a target whose final path component is a symlink, checked with `lstat`/`symlink_metadata` before any target runtime is constructed, rather than following it to a regular file. An explicit existing-file check uses that non-following metadata. The default target is checked when its final component exists; a missing default remains create-capable. A symlinked target refuses even when it points to an otherwise valid dedicated map. An independent byte copy of a protected database, at a new file identity, remains admissible under A9. Parent-directory aliases still use the existing normalization rule. This is a preflight rule, not a claim that a path cannot change afterward.

3. **Residual threat.** A local writer can swap the target between the courtesy preflight and SQLite's actual open, so the currently shipped fence does not establish E7's opened-handle exclusion.

4. **Deferred handle-level proof.** Issue #3552 owns the full guarded native VFS design for code-pack target opens. It must cover main, journal, WAL, and SHM, including the `xShmMap` path and native locking semantics, and prove no-follow traversal plus the identity of each actual retained OS handle before first use. On Unix this means handle-relative no-follow traversal and `fstat` identity; on Windows it means reparse-safe opens and identity from the retained handle. The design must specify refresh or snapshot semantics for protected companions that appear or change during an attempted open. A path re-probe after delegating a native open is not an opened-handle proof.

The #3552 acceptance suite must place a deterministic barrier after courtesy preflight and before SQLite open, then swap the target main and each companion to a production hard link and to a symlink or reparse point. Each arm must refuse before migration or companion writes. It must also refuse an unrelated hard-linked writable companion with link count above one, admit an independent byte copy and an ordinary dedicated map, and test identity refresh after a refused open. For every refusal, compare the protected main/WAL/SHM presence, bytes, size, and modification time before and after. Exercise pooled writers, pooled and standalone readers, and any other code-pack-created connection on Unix and Windows; one unguarded open invalidates the proof.

This amendment narrows E7's present-tense implementation claim. It does not remove E7's target opened-handle security objective or A9's production deny set. The three analysis verbs remain unshipped, and their read-only constructor and migration-free requirements remain target design rather than a claim about current runtime behavior.

## Acceptance for this amendment

- A `code.ingest` explicit target that is a final-component symlink to a dedicated map is refused before runtime construction; a symlink to a protected file is refused as well.
- A separate byte copy of a protected database at a new inode/file identity remains admissible, subject to the existing map-target rules.
- The source and public documentation describe the current check as a path-level courtesy preflight and name the local swap race. No test or documentation claims that #3552's handle-level guarantee is already shipped.

## References

- ADR-085 Amendment 4 E7 and Amendment 9 A9
- `crates/khive-pack-code/src/db_target.rs` and `crates/khive-pack-code/src/pack.rs`
- `crates/khive-db/src/pool.rs` SQLite connection opens
- #1855 and #3552

## Amendment 12 (2026-09-28): native opened-handle proof for code-map databases

**Amends:** Amendment 4 E7's VFS mechanism and WAL/SHM side-effect allowance, and Amendment 11's deferred implementation description. **Retains:** E7's production exclusion objective, Amendment 9's complete configured production deny set and populated-prior-map acceptance, the explicit-target existence rule, and the analysis verbs' read-only/no-migration design.

E7's “thin VFS wrapper layered over the platform's default VFS” is replaced by a guarded **native** VFS for code-pack target runtimes. Its own `xOpen` opens and retains the actual OS handle for each target main database and rollback journal; its file I/O, SQLite-compatible rollback locking, sync, close, `xAccess`, and `xDelete` operate on those proved handles or a proved parent. Delegating an open or lock to the platform default VFS and re-probing a pathname is not proof. Every code-pack-created pooled writer/reader, replacement reader, standalone writer/reader, writer task, checkpoint/diagnostics handle, guarded transition handle, and future analysis connection must select this VFS; an unguarded target connection is a fence failure.

The steady-state code-map target uses rollback `journal_mode=DELETE`, confirmed before any schema write, rather than WAL. A fresh map uses DELETE from its first write. WAL and SHM are **not opened by construction** after the guarded transition: the target header's format bytes 18–19 must both be 1 and neither `-wal` nor `-shm` may be present at every steady-state open. An unexpected WAL `xOpen` or `xShmMap`/`xShmLock`/`xShmUnmap` must fail closed; every shared-memory callback, including the void `xShmBarrier`, is counted as a contract violation. A change of journal mode while the target is open is refused. The code-map-only rollback configuration does not change ordinary khive databases. Separate code-map runtimes remain able to read and rebase concurrently under the ordinary bounded busy timeout; exclusive WAL is never their steady-state mode.

A populated prior code map in persistent WAL mode remains an accepted explicit target under Amendment 9. Before the rollback constructor or any SQLite access, a separate guarded, **quiescent transition** uses no-follow `fstatat` through a pinned parent directory (and the equivalent reparse-safe native metadata operation on Windows) to inspect the main name and every existing `-wal`/`-shm` sidecar name, checking type, link count, and identity against every protected production member and companion. This early companion check must not open a sidecar: opening and then closing a production alias could release POSIX locks held by another connection in the same process. A symlink/reparse point, a multiply linked writable sidecar, or any cross-role protected identity found at admission refuses before mutation. The transition selects the guarded VFS; it sets `locking_mode=EXCLUSIVE` **before first WAL access** so SQLite uses an in-memory WAL index and never calls `xShm*`. Its actual main and `-wal` opens are performed by that VFS's attested `xOpen`, with no default-VFS fallback; the pre-open name check is not substituted for proof of the opened handle. A target held by another client returns BUSY promptly; the transition does not wait. After durably checkpointing the WAL, and while its EXCLUSIVE lock remains held, it repeats no-follow metadata checks on any remaining `-shm`/`-wal` names against their earlier attestation and Amendment 9's protected set. Every transition-refusing check—including admission identity, guarded-VFS attestation, EXCLUSIVE ownership, and prompt BUSY detection—must finish before `PRAGMA journal_mode=DELETE`; after that PRAGMA succeeds, while the EXCLUSIVE lock remains held, the transition re-checks any remaining `-wal`/`-shm` names—including a pre-existing `-shm` from the map's WAL past—against their earlier attestation and Amendment 9's protected set, then unlinks only attested target sidecar names through the pinned parent before closing the transition handle and performing post-reopen proof. Any sidecar unlink is confined by pinned-parent identity to a `-wal` or `-shm` name in the target directory. A competing opener planted between checkpoint and cleanup may cause at most such a target-directory name to be unlinked; Amendment 9's protected cross-role collision refusal and the pinned-parent identity bound ensure that no protected production member or companion loses its name or data. A deterministic competing-opener test must assert this bound. A refusal before the PRAGMA reports an incomplete transition with every sidecar still in place. A failure at or after the PRAGMA reports a **PARTIAL** target, never an all-sidecars-intact state: the checkpoint is durable; SQLite may have removed the WAL, and the main may have entered rollback mode. If the PRAGMA succeeds, SQLite's WAL removal and the main's rollback mode are retained even if a later proof fails. After a successful mode switch, it reopens under rollback mode and proves the header/sidecar/zero-`xShm*` invariant before migration or ingest; failure of that post-reopen proof refuses ingest on the partial target. A failed transition leaves the protected production files unchanged and does not expose an unproved target handle. The transition's temporary WAL open is the sole code-map exception to steady-state WAL absence, not an ordinary code-map writer route.

For each governed main, journal, or transition-WAL open, traverse **every** target path component without following a symlink or Windows reparse point. Unix uses descriptor-relative no-follow directory/leaf opens and `fstat` on the exact fd SQLite retains; Windows uses reparse-safe directory-relative native opens and reads type, link count, volume, and file ID from the exact retained HANDLE. Refuse before SQLite reads, writes, maps, truncates, locks, or otherwise uses an unproved handle. At each admission, obtain a **best-effort path-stat snapshot** of Amendment 9's configured production main and event names plus all presently existing `-journal`, `-wal`, and `-shm` companions, using a pinned parent and no-follow metadata reads. A production inode renamed away from every configured path, or swapped after that stat sample while its production handle stays open, can be absent from this snapshot; the tracked opened-handle registry planned in follow-up #3593 is required to close that gap. A role-tagged admission ledger combines those sampled production-path identities with the exact `fstat`/native-handle identities of every handle the guarded VFS itself opens. Reject a target-to-production cross-role identity match regardless of suffix pairing; a second handle to the same code-map target is not itself a production collision. Refresh the path-stat samples on each new open and after a refusal. On Unix, pin each admitted physical object through an exact retained handle, including after its name is changed. Retire it only when its native link count is zero and every guarded open has closed; close the pin through the native lock-safe deferred-close rule. An inode recycled after that object is gone identifies a new object, not a lifetime cross-role alias. Windows retains the existing process-lifetime guarded file-ID ledger; this Unix inode-recycling correction does not change its guard contract. Refuse when a protected member cannot be read safely or changes during admission. Before first main-file use, inspect existing target companions with pinned-parent no-follow metadata, without opening them as a preflight. Production connections opened through the ordinary SQLite VFS do not expose stable native handles to this guard: a production inode moved away from every configured path, or swapped between a production-path stat and its use, can escape the sampled set. This admission is therefore **not** an exact lifetime registry. Follow-up #3593 remains open for exact production-handle coverage. Moving code-map opens into a separate worker process (F5) is the recorded retirement path for both this inexact in-process set and the quarantine below.

_Corrected in place on 2026-10-05._ The sentences in the paragraph above, from "On Unix, pin each admitted physical object" through "does not change its guard contract", replaced the accepted wording "while retaining the guarded-handle identities for their process lifetime". A guarded identity is a device and inode number, and after a file is gone that number can be reused by an unrelated file, so a ledger of numbers held for the process lifetime could refuse a new file for an old file's role. A pinned handle keeps the object itself, so while the guard holds it the number names that one object, and the guard retires it only once the object has no links and no guarded open remains. Windows is unchanged.

If a target path changes from a harmless name at its pre-open check to a protected alias before the guarded `xOpen`, post-open comparison uses the exact retained handle. On a protected-identity match, the VFS refuses before SQLite uses the handle and retains that fd in a bounded, process-lifetime quarantine rather than closing it: closing an aliased fd can release POSIX locks held by a production connection in the same process. This quarantine is solely for the stat-to-open swap; pre-open companion refusals and no-follow open failures do not consume it. At the quarantine cap, every further code-map open fails **before another OS open** with a typed error that names process restart as the remedy. An attacker who repeatedly wins this race can therefore deny code-map availability until restart. Non-adversarial runs must end with quarantine occupancy zero.

An explicit `db` receives **no parent-path canonicalization transform** before the guarded open. Any symlink/reparse component refuses even if a path-level courtesy check would normalize it. Only an omitted-`db` default may canonicalize its configured parent **once** before creating/opening `<path>/.khive/code-map.db`, for platform aliases such as macOS `/var/folders`; native no-follow traversal and retained-handle proof still follow. Controls must fail if the default transform is removed or applied to an explicit `/var/...` path.

Deterministic barriers between the pinned-parent preflight and guarded SQLite open must make a target main or journal swap to a protected hard link, symlink, or reparse point refuse before schema or companion mutation; the transition repeats these arms for existing WAL/SHM sidecars, including a sidecar hard-linked to any protected member. Every identity-swap arm that reaches post-open protected-handle refusal raises quarantine occupancy by exactly one, and a same-process production write after refusal must still succeed, proving its POSIX locks survived. A no-follow symlink/reparse refusal with no opened handle leaves occupancy unchanged; it also must be followed by a successful same-process production write. Run to the cap and prove every further code-map open fails closed with the typed restart remedy. A multiply linked unrelated writable journal/WAL/SHM refuses; an independent byte copy and a dedicated map succeed. Compare protected main/WAL/SHM presence, bytes, size, and modification time across refusals. Exercise all code-pack connection classes on Unix and Windows, protected-set refresh, hot rollback-journal recovery, prior-WAL checkpoint/DELETE transition, native locking, and the zero-`xShm*` trap/absence controls. The entire non-adversarial suite must finish with quarantine occupancy zero; any occupant is a finding. Mutating the code-only rollback choice, removing a pre-open sidecar check, closing a refused aliased handle, or bypassing the cap must make a named arm red. Existing Amendment 8's three concurrent-rebase acceptance arms run under DELETE plus busy timeout; a BUSY outcome is a retry defect to repair, never a reason to restore WAL. The measured same-fixture two-runtime ingest wall times are **WAL: median 190 ms (range 135–333 ms); DELETE: median 242 ms (range 197–632 ms)** on `code-map-a8-32` with 32 commits, across six runs paired within process on the Mac mini under concurrent cargo build load (1-minute load 3.9–5.6).

The three analysis verbs remain unshipped. Their later read-only constructor must use this native guard and never run migration; an old WAL map requires the separate guarded transition before a read-only analysis open. No change to Amendment 9's configured production member definition is made here.

### Proposed clarification: guarded identity lifetime

**Status update (2026-10-18)**: Evaluated with Amendment 13's revision of the same date, as this section requires. On Unix it remains superseded by the 2026-10-05 in-place correction to Amendment 12's admission-ledger paragraph: retirement at link count zero with every guarded open closed. For Windows, that revision proposes withdrawing it. The withdrawal takes effect when Amendment 13 is ratified, that is once the worker exists; until then the 2026-10-05 status below governs. Under the worker architecture the process-lifetime guarded file-ID ledger belongs to the worker process, and restarting the worker retires it without touching the parent's descriptors (Amendment 13, W6). At the census revision the Windows ledger refuses a recorded file ID only when it reappears in a different role (`khive-db/src/code_map_vfs/ledger.rs:28–41`); a reappearance in the same role is admitted, and a recorded file ID that reappears as a protected production file fails the production-set preflight instead (`khive-db/src/code_map_vfs/mod.rs:639–645`). The same ledger appends one record per guarded open and never releases one (`ledger.rs:66–70`, `:83–84`), so in rollback DELETE mode it grows with every write transaction's journal; W6 bounds it. Windows acceptance keeps Amendment 12's accepted controls and adds Amendment 13's restart-reclaim arm. The retained-owner lifetime arms below belong to this proposal and leave with it on Windows.

**Status update (2026-10-05)**: On Unix this proposal is superseded by the in-place correction to Amendment 12's admission-ledger paragraph, dated there: each admitted object is pinned through a retained handle and retired at link count zero with every guarded open closed. The sentence quoted below no longer appears in that paragraph and is kept here as history. For Windows the proposal stands as written, Proposed, to be ratified or withdrawn with Amendment 13; until then Windows keeps the process-lifetime guarded file-ID ledger.

**Original status (2026-09-30)**: Proposed; pending ratification with Amendment 13. The accepted Amendment 12 wording remains binding until this clarification is ratified. Upon ratification, it replaces exactly this sentence in Amendment 12's admission-ledger paragraph:

> Refresh the path-stat samples on each new open and after a refusal while retaining the guarded-handle identities for their process lifetime.

The proposed replacement sentence is:

> Refresh the path-stat samples on each new open and after a refusal while retaining each guarded-handle identity until its last actual native handle close.

All other accepted Amendment 12 sentences remain unchanged. The proposed lifetime semantics are: A guarded file identity remains in the live, role-tagged set until the **last native handle close**, rather than for the process lifetime. Each admitted descriptor has a lifetime witness; additional descriptors and duplicated native-handle owners count independently or share an explicit reference count. Closing the first descriptor or completing SQLite's logical `xClose` must not retire an identity still retained by a deferred native close or another owner. Once every native owner has closed, the identity leaves the live set before another admission can classify a recreated file under a reused device/inode or platform file ID. A historical identity alone is not evidence that the new file has the historical role.

Protected-alias quarantine entries never leave during the process lifetime, and the quarantine cap and restart remedy remain unchanged. The configured-production path-stat snapshot is refreshed with the same checks as before. This clarification neither implements nor narrows the exact production-handle registry gap tracked by #3593: a production file moved away from all configured names, or swapped after the path-stat sample while its production handle remains open, can still escape the sampled set.

Acceptance must cover multiple admitted descriptors, duplicated owners and deferred native closes. A remaining native owner must keep its identity live after another owner's logical or native close. After the last native close, recreating a file with a reused identity and a different role must succeed without a stale-role refusal; the existing protected-identity swap refusal must still fail when that protection is removed. Quarantine entries must remain live. This lifetime correction and Amendment 13 must be evaluated in the same document revision.

## Amendment 13 (2026-09-30, revised 2026-11-16): code-map descriptors owned by a worker process

**Status: Proposed. Signed off 2026-11-16 (#6729) for implementation on a branch. Architecture: worker-process isolation, option 3a below, one worker per governed map within one parent, selected for this revision on 2026-11-16. Nothing in it takes effect before ratification, which requires the executed acceptance below.**

**Amends:** Amendment 12's connection coverage and lock-ownership boundary, and the meaning of "process" in its quarantine, registration and identity-ledger lifetimes. **Retains:** its native opened-handle proof, complete configured production deny set, rollback DELETE posture, guarded prior-WAL transition, read-only/no-migration requirements and protected-alias quarantine. The configured-production path-stat snapshot and the exact production-handle registry gap in #3593 are unchanged. The historical `(F5)` parenthetical retained in Amendment 12 names this retirement path, not a section in Amendment 5 or ADR-135.

### State at this revision

Amendment 12's native VFS is shipped code. It landed in #4129 on 2026-10-05; #3674, the pull request this amendment's first text named, closed unmerged the same minute. `code.ingest` constructs the guarded code-map runtime directly, and the registered-map read path (#5628) opens maps through the same VFS.

The first text held production implementation, #3674 included, until this amendment's executed acceptance had passed and the amendment was ratified on that evidence, and stated that the hold applied from the time the text landed, whatever the amendment's Proposed status. The ADR index states that a Proposed record is awaiting ratification and that an accepted decision is in force (`docs/adr/README.md`, ADR lifecycle). #4129, an implementation of the same VFS, merged on 2026-10-05 while that text stood, and Accepted Amendment 14 states that it does not release the hold. This revision records those facts and changes neither: it does not release, restate or narrow the hold, and it does not ask to remove the shipped in-process VFS. Whether #4129's merge sat within the hold is for the maintainer at sign-off. The worker is production implementation under the same hold, so it merges only after ratification (Sequencing).

At the census revision a Windows code-map pool cannot complete construction. Pool construction reads the opened file's identity through SQLite's Windows handle control (`SQLITE_FCNTL_WIN32_GET_HANDLE`, `khive-db/src/file_identity.rs:101–121`, called from `khive-db/src/pool/construction.rs:191–192` for every file-backed pool, code maps included), and the native VFS answers that control with not-found: its file-control callback handles the persist-WAL query, the journal-mode pragma check and, on Unix, the move check, and returns not-found for every other control (`khive-db/src/code_map_vfs/callbacks.rs:401–462`). The Windows CI lane runs a fixed set of targeted tests and none of them constructs a code-map pool, so this follows from the source and has not been executed (#5722). That historical prerequisite applied to both recorded architectures. At the current-base revision `cf9ceab64a1854ad0e2d7540f970b1eb289465b1`, the native VFS implements the Windows handle control; the current-base census delta below records the changed source and constructor witness. That source change does not prove executed Windows acceptance of this Proposed worker contract.

The first text also ordered a routing-free lock-mechanism probe before the architecture choice. Its results are recorded below. Sign-off now reviews the selected option 3a contract and its acceptance; it does not reopen a choice between one shared worker and one worker per map. The probe record is evidence for the lock mechanism, not executed acceptance of the worker (Sequencing).

#5718 merged as `1c1a0728e` and provides a process-wide `code_map_vfs` block in `db_diagnostics` with `quarantine_occupancy`, `quarantine_cap`, `registrations` and `registration_cap` (diagnostics, no contract; `crates/khive-db/src/diagnostics.rs:1509–1523`); W6 adds the ledger's entry count. Readings of that block on a running daemon inform when the worker is scheduled (#5614). The new source references for this block and the ownership, attachment and channel decisions below use `c4db30506472064fbb509f2e234b5f3ba42f7379`; the census and its existing references retain their separately stated revision.

### Lock interference this amendment answers

POSIX record locks belong to a process, not to the descriptor or SQLite connection that acquired them. Closing **any descriptor** for that file can remove that process's locks. A process-owned whole-file `F_UNLCK` can remove another client's locks without a close, and a same-process `F_RDLCK` over a previously write-locked range can replace its lock type. SQLite's default Unix VFS calls `posixUnlock` from `unixUnlock`; when its last shared holder releases, it issues `F_UNLCK` with `l_start = l_len = 0`. SQLite's own source states that closing any descriptor for an inode releases the process's locks and that its pending-descriptor list defers only the closes SQLite itself controls (bundled `libsqlite3-sys` 0.38.1, `sqlite3.c:41431–41442`). Its default `unixOpen` can also take a main-database descriptor from that private list without a new OS open (`findReusableFd`, `sqlite3.c:46556`, called at `46818`, before `robust_open` at `46868`). A separate native lock table cannot see a default-VFS owner's bookkeeping. These hazards apply on **every Unix host gate, including Linux and macOS**, in both directions between guarded and ordinary clients. Windows byte-range locks belong to the handle that acquired them, so on Windows another handle's close releases nothing.

### Decision: worker-process isolation

Each governed map within a parent process has its own dedicated worker process. Opens, closes and unlocks in the parent or a different map's worker cannot release or replace locks that worker holds, because the locks belong to a different process. Inside a worker the same per-process hazard applies to its own opens, so W3 still governs every open that can name an inode whose locks that worker holds. Restarting one map's worker reclaims that worker's quarantine, registrations and identity ledger without restarting the parent or another map's worker.

Why this option and not the others below:

- **Option 0 (withdraw)** is no longer free: the native VFS has shipped, and withdrawing it reopens the gaps Amendment 12 closed.
- **Options 1 and 2** keep guarded and ordinary clients in one process, so their safety depends on finding and gating every opener in that process. The census below classifies more than thirty opening classes at one revision and still leaves eight residual categories open (imported aliases, macro expansions, transitive dependencies, dynamic subprocesses among them). A source census cannot prove completeness, and a mechanical gate would have to close every one of those categories to support a whole-process claim.
- **Option 3a** confines that obligation to the worker's own code, where every open that can name a map goes through guarded admission (W3). The parent census still matters for routing and for the parent's own refusals (W2), but a parent opener the census missed cannot release a worker lock.
- **A per-map circuit breaker inside the current process** was considered for the quarantine-cap symptom (#5614) and rejected: it is a partial fix with a documented residual, needs its own amendment, and the rate of the trigger it answers is unmeasured.

This revision proposes **one worker per governed map within one parent**. A parent is any khive process that needs a governed map, including a daemon or a daemonless `kkernel` invocation. The parent starts the map's worker at its first operation. A CLI invocation owns its workers even while a daemon is running; workers are never shared between parents. Two parents using one map remain two processes using one map, governed by Amendment 12's SQLite-compatible native rollback locking, bounded busy timeout and quiescent-transition refusal while another client holds the map (`docs/adr/ADR-085-code-pack.md:1910–1914` at the source revision). The local map construction in `code_audit.rs`, the repository command's local runtime and the snapshot-copy path before read-only construction must all dispatch to the worker for that invocation's map key; none may borrow the daemon's worker.

**Stable map key.** Within a parent the key is the canonical target pathname, not a file identity, device/inode pair or generation. A registration name resolves to this same pathname key; registered reads and explicit or default `code.ingest` targeting that path share one worker reservation. For an explicit target, resolve the directory pathname and append the exact final component without opening or following the main file. Preserve the original explicit target spelling and pass it to the worker's Amendment 12 admission; do not substitute the normalized routing key for that spelling before its component and no-follow checks. For an omitted target, use the canonical ingest root plus the literal `.khive/code-map.db` suffix, retaining the existing default route's permitted directory creation. Computing a key opens no governed map and grants no admission authority. All target validation, pinned-parent and opened-file identity checks, and protected-set revalidation still run as required by Amendment 12. This routing rule does not adopt the separate Proposed Amendment 17 configured-parent lifetime change. Replacing the main file or the pinned parent at the same canonical pathname leaves the worker key unchanged. The parent retains that key for the registration or operation; a newly observed native identity cannot mint another worker. Distinct registration names for the same pathname resolve to the same key; arbitrary same-inode aliases are not used as a second keying rule. Spellings are not folded: on a filesystem that ignores letter case or normalizes names, two spellings of one file's final component are two keys, so one parent can run two workers on one map, each admitted separately under Amendment 12 and coordinated by Amendment 12's cross-process locking as two processes would be; the shared-reservation rule above holds per spelling.

**Parent capacity and idle retirement.** A parent reserves at most **16 live worker slots**, counting workers that are starting, running, stopping or awaiting reap. It atomically owns one reservation per key: a request for an existing key reuses that reservation, and concurrent first requests cannot start two workers for it. The reservation is not released, and no replacement for that key may start, until the old child has exited and been reaped. An idle worker yields its slot: when a new distinct key finds no free slot and a worker is idle in the sense defined below, the parent begins that worker's retirement at once, choosing the longest-idle worker, and the new key is admitted to the slot after the retired child has exited and been reaped, through the same retire, exit and reap path as idle retirement. A new key waits, within its existing deadline, for a slot already being released by retirement, failed-first-admission cleanup or cap draining. Only when every slot is held by a worker that is neither idle nor releasing is the new key refused, before spawn or map dispatch, with the typed `worker_capacity_exceeded` error naming the limit `16`; the error contains no input path. Such a refusal takes no reservation and starts no child, so the failed-first-admission cleanup below has nothing to reap and the request completes with the refusal at once. Busy workers are never evicted to admit another key, and an existing key needs no extra slot. Parent runtime construction admits registrations in turn within the bound (W1): each successful initial admission leaves an idle worker that yields its slot to a later registration, so a parent with more than 16 registrations constructs, and a later read of a registration whose worker has retired lazily starts a successor that compares against the identity recorded at construction. A registration that still obtains no slot within the construction deadline is rejected as any other failed admission is, logged and left out of the registered set, and construction proceeds with the rest. This parent bound is separate from the worker-local quarantine, registration and ledger limits. A worker whose first target admission refuses, or that has admitted no map when its initial request settles, does not enter the idle-retirement interval: it settles the refusal and any cleanup, closes, exits and is reaped promptly. Queued requests for that key cannot keep such a worker alive; they receive the refusal or wait for a successor within their existing deadlines. Its slot remains counted until reap, and the parent completes the failed first-admission request only after that reap and reservation release; process exit alone is not a settled refusal. Distinct hard-link aliases and nonexistent leaf names therefore cannot retain failed workers for 60 seconds. A successful admission, rather than a requested pathname or a spawned child, is required before ordinary idle retention applies.

A worker retires after **60 continuous monotonic seconds idle**, or earlier when it yields its slot to a new key as above. Idle means no queued or in-flight operation, open transaction, pending reply or source-byte exchange, and no cancellation or settlement cleanup still running. New work admitted before retirement begins resets the idle interval. Retirement and admission serialize on the key: once retirement begins, later work waits for that key's child to exit and be reaped before a successor can start, subject to the request's existing deadline and W7 failure handling. Close the connections, exit and reap before freeing the slot. A later request lazily starts a worker for the same key; it does not refresh the parent's registered file identity. Failed-first-admission cleanup above, slot yielding and cap-triggered draining under W6 do not wait for this idle interval. The values 16 and 60 seconds are choices of this Proposed contract, not measured capacity or latency claims.

Each worker also ends with its parent, whether the parent exits, crashes or is killed. Each worker holds the read end of a lifetime pipe whose write end only that parent holds; no unrelated child or sibling worker inherits a write end. Create these endpoints with close-on-exec or non-inheritable attributes atomically where the platform offers them; otherwise serialize creation and child spawning so no inheritable interval is exposed. End of file makes the worker close its connections, rolling back an open transaction, and exit. W7 governs failure and restart before that point. Worker-local resource lifetime ends when that worker exits, including idle retirement; parent registration lifetime is unchanged. A shared worker would let one map's exhausted cap, crash or restart interrupt operations on other maps in that parent. Option 3a supplies the required failure domain at the cost of a process and supervised lifecycle per active map, within the stated bound; acceptance records resident memory and first-operation latency without a threshold.

### Worker contract

- **W1 Ownership.** Every open of a governed code-map main database, rollback journal, transition WAL or companion, and every header or identity read of one, happens in the worker through guarded admission: construction, pooled and standalone connections, replacement readers, writer tasks, checkpoint and diagnostics handles, schema and snapshot inspection, the prior-WAL transition, migration, ingest, rebase, registered-map loading and per-read revalidation, replacement verification and snapshot copies. During parent runtime construction, the parent parses each map registration and asks that key's worker to perform its initial guarded identity/header admission. The worker returns owned identity metadata, never a descriptor, and the parent records that exact identity with the registration before construction succeeds. Construction admits registrations in turn within the parent bound, idle workers yielding their slots (Parent capacity and idle retirement, above). Worker startup or admission failure, including a slot not obtained within the construction deadline, fails that registration: it is logged and left out of the registered set, construction proceeds with the rest, and it follows the failed-first-admission cleanup rule. Later registered reads send the recorded identity to the same-key worker for comparison with its guarded observation; restarting or idling out the worker never replaces the parent's recorded identity.
- **W2 Parent dispatch or refusal, a routing contract.** Every supported operation on a code map runs in the worker, and the parent's database constructors and helpers refuse the map targets they can recognize; this is a routing contract, not a guarantee that the parent cannot change a map, because a process with the same filesystem authority that opens files with stock SQLite can still write a map or its companions by the routes named under Exclusions below.
  - _Routing._ A parent operation that needs a map sends it to the worker (W4). The parent opens its own databases only through one admitted-target constructor, and a map path, a registered map, a path of the default-target shape or a file that carries the code-map application ID cannot become an admitted target. Map handles and the operation protocol are a separate interface. The code-map constructors and the registration of the guarded VFS are reachable only from the worker; the parent never registers or selects that VFS. Every parent opener, SQLite and raw, is classified behind one gate, and a new unclassified opener fails it (acceptance, New opener).
  - _Windows code-target equality preflight exception._ The code-target equality proof in `khive-pack-code/src/db_target.rs:102` may remain in the parent, using the existing `same-file` Windows identity handles. This is an explicit W2 parent-inspection exception: the handles are temporary, export no descriptor to the worker and cannot release another handle's Windows locks. The proof grants no map admission; the worker independently reads the opened target's identity through guarded admission. Registered-map initial preflight and per-read identity checks remain worker-owned under W1, and this exception permits no parent map data or SQLite open.
  - _Known-map refusals._ The admitted-target constructor refuses a target on two checks, neither of which reads the worker's state. The first compares the target's canonical path with the registered map paths and with the default-target shape (`<directory>/.khive/code-map.db`). The second reads the main file's application ID (header bytes 68–71) with a plain file read, beside the file identity the constructor already takes before the open (`khive-db/src/pool/construction.rs:143–149`), and refuses the code-map value. The worker writes that application ID into every map it creates or ingests, in the first transaction it commits to a new map; at the census revision no khive database sets one. A read-only pool's raw header read, which at the census revision opens the file a second time and reads 20 bytes (`khive-db/src/pool/connection_setup.rs:168–178`), reads 72 bytes from the descriptor whose identity the constructor sampled. After SQLite opens the file and before the connection's first SQL statement, every connection the constructor opens, and every standalone reader later opened on the pool it returns, binds the opened file to that sample: the writer, each pooled reader, initial and replacement, and each standalone reader (`open_standalone_reader`, `khive-db/src/pool/pool_impl.rs:887–918`, which at the census revision already compares the opened connection's file identity before its pragmas, `:908–911`). The opened file's identity must equal the identity taken before the open, and when the path did not exist before the open, the opened file must be empty. At the census revision the following differ, and each changes. The writer binds after the `main.sqlite_master` read that can play back a hot journal (`construction.rs:166–170`, before the binding at `:185–198`); when a WAL ceiling is set, a `PRAGMA page_size` query precedes that read (`:159`), and that query takes no lock (`sqlite3.c:144484`, `:145256–145261`). A path that was absent before the open is never compared (`:185`). A pooled reader runs its connection pragmas before it compares the opened file's identity (`khive-db/src/pool/pool_impl.rs:633–644`, pragmas at `connection_setup.rs:242`). Standalone read-only snapshot connections compare no identity and are opened directly by the migration reads (`connection_setup.rs:159–166`, called at `khive-db/src/migrations.rs:1528`, `:1535`, `:1555`, `:1574`); under this amendment they are opened only through the admitted-target constructor, which refuses the maps it recognizes (W2; exclusion (e) names the ones it does not), and a map's snapshot inspection runs in the worker under W1. The binding compares observations taken at different instants, SQLite's moved-file check and a path stat (`khive-db/src/pool/identity.rs:213–239`, `:125–128`), so it refuses a swap it observes and does not prove that none happened. These refusals are safeguards for the targets they recognize: a passing path or header check does not establish that a file is an ordinary database.
  - _Attachment._ A connection may attach only a file that the backend configuration names as a backend, and only while its canonical path and file identity equal those the admitted-target constructor recorded when the runtime constructed that backend. That construction applied the known-map refusals above, so a configured backend that is a map never becomes attachable (_Configured map backends_, below). Every other file is refused, governed maps included. SQLite calls the authorizer while it prepares an `ATTACH` statement (`sqlite3.c:125898`), and it opens the file and reads that file's schema only when the statement executes (`attachFunc`, open at `:125700`, schema read at `:125752`), so the comparison is made when the statement is prepared; exclusion (h) names the interval this leaves. Amendment 9's complete configured-production deny set remains in force. The reason for this scope is to keep the accepted ATTACH-based multi-backend design of ADR-005 (`docs/adr/ADR-005-storage-capability-traits.md:374–377`) and ADR-009 (`docs/adr/ADR-009-backend-architecture.md:167–173`) possible; neither ADR is amended. Each connection's authorizer admits a literal file name only after that comparison succeeds, or the empty string literal, which is the temporary database SQLite's own `VACUUM` attaches (`ATTACH '' AS vacuum_<n>`; bundled SQLite 3.53.2, `sqlite3.c:161481`, `:161520`). SQLite passes the authorizer the literal file name, or no name when the argument is not a string literal (`sqlite3.c:125889–125899`), and `VACUUM` runs its attachment through the same authorization (`sqlite3AuthCheck` skips only when no authorizer is set, during schema initialization or inside a special parse, `:126332–126346`; `VACUUM` saves and restores connection flags but not the authorizer, `:161446–161698`). Thus an unconfigured path, a non-matching alias or identity, a URI, a bound parameter or an expression is refused before SQLite opens the file, and `VACUUM` keeps working. For the parent's pre-attach identity comparison, `database_file_identity` reads Unix metadata (`crates/khive-db/src/file_identity.rs:25–34`), obtaining device and inode without opening a candidate descriptor; it must not be replaced by a raw open-and-close that could name a map. Its Windows branch reads attributes through a temporary handle (`:57–70`), which W3 permits because closing that handle cannot release another handle's locks. Identity inspection inside the worker still obeys W3. The comparison is an admission check, not a claim that a later pathname swap is impossible. After an `ATTACH` statement executes and before the connection runs any other statement, the connection binds the attached schema's opened file to the recorded identity by the means the main-file binding uses, with the attached schema's name in place of `main`: SQLite's moved-file check and a path stat on Unix (`khive-db/src/pool/identity.rs:213–239`, `:125–128`), and the handle file control on Windows (`khive-db/src/file_identity.rs:128–149`). A mismatch detaches the schema and refuses the statement. Unlike the constructor's binding, this one runs after SQLite has read the attached file's schema, because the open and that read are one step inside `ATTACH`. The binding needs a point between the `ATTACH` statement and the next statement where khive runs code. The script route has none: `execute_script` hands the whole text to rusqlite's `execute_batch` (`khive-db/src/sql_bridge/sqlite_writer.rs:519–537`), which prepares and steps each statement in turn inside that one call (rusqlite 0.40.1, `src/lib.rs:548–565`). The bridge writer's single-statement route (`sqlite_writer.rs:374`) has one, because rusqlite refuses a multi-statement text there before anything executes (`src/lib.rs:792–797`). So the authorizer admits a configured backend's file name only while the connection is executing one statement through that route, which marks the connection for the call and runs the binding before the call returns. On every other route (a script, a statement batch, or a caller's raw handle of a khive connection) a named `ATTACH` is refused when it is prepared, so the binding has one route to cover. The empty-name attachment that `VACUUM` makes stays admitted on every route. The attached-database limit is not the mechanism: at zero it also refuses `VACUUM`'s attachment, with no exception for it (`sqlite3.c:125651`), and khive runs `VACUUM` on its ordinary databases (`khive-storage/src/sql.rs:39–51`). SQLite keeps one authorizer per connection. At the census revision khive installs one at three sites, and each refuses every action, `ATTACH` included: `deny_retired_writer` (`khive-db/src/pool.rs:237–239`) on a retired writer (`pool/acquisition.rs:481`) and on an in-memory retirement connection (`pool/construction.rs:151`), and a deny-all closure on an in-memory replacement during migration settlement (`raw_migration_settlement.rs:30`). The attachment rule is installed on every other connection, and any authorizer khive adds carries it. Settlement clears the authorizer and then runs only `ROLLBACK` and close (`khive-db/src/connection_settlement.rs:18–37`). No `ATTACH` statement is in khive's production code at the census revision; a caller's SQL can attach an arbitrary file through the bridge writer (the census row for writable SQL `ATTACH`). Stopping that behavior except for a configured backend is an intended change, preserving the multi-backend path while refusing other files. Six `ATTACH DATABASE` statements are in four test files. Two check that the bridge reader refuses them (`khive-db/src/sql_bridge_tests.rs:331`, `:586`). The other four attach a scratch file through a pool or bridge-writer connection (`khive-db/src/pool_tests.rs:673`, `:945`; `sql_bridge/direct_busy_tests.rs:135`; `writer_busy_fixture.rs:358`) and are rewritten (acceptance, Attach).
  - _Configured map backends._ Amendment 2's B7 documents an interactive read path that declares a map as the `main` backend of a dedicated `[[backends]]` configuration. At the census revision the runtime opens every declared SQLite backend through the ordinary pool constructor, a declared map included (census row, configured backend construction). Under this amendment the runtime constructs configured backends only through the admitted-target constructor, so a declared backend that the known-map refusals recognize is refused before SQLite opens it, with an error that names the backend and the registered-map read path, and the configuration does not load. This retires B7's dedicated-configuration read path; map reads go through the registered-map read path (ADR-139), whose opens run in the worker (W1). Dispatching a configured map backend's generic graph reads to the worker was considered and is not proposed: it would move the generic read surface into the worker and widen the opener population W3 governs. A declared map that the known-map refusals do not recognize is exclusion (e).
  - _Worker refusals._ The worker refuses a map whose main file has a link count other than 1, at every open. ADR-139 Amendment 2, merged after the census revision (#5719), requires this for registered maps; this amendment extends it to every map the worker opens. At the census revision the guard refuses a multiply linked writable companion but not a multiply linked main file (`khive-db/src/code_map_vfs/mod.rs:751`). For a map with one link that a parent wrote in WAL mode, the worker's ingest open runs the quiescent WAL transition. It refuses while any other code-map connection, the worker's own included, holds the map (`khive-db/src/code_map_vfs/transition.rs:199–215`), takes its exclusive lock with no busy wait (`transition.rs:258–278`), and refuses when another client still holds WAL frames or a checkpoint does not cover every frame it sees (`transition.rs:296–311`); otherwise the map keeps what the parent wrote and returns to rollback mode. A rollback-mode open that does not run that transition first, such as a read-only open, refuses the map in admission when a WAL or shared-memory companion exists (`khive-db/src/code_map_vfs/mod.rs:650–655`) and refuses a WAL header (`mod.rs:766–776`). The worker's guarded-identity ledger is not a parent check: it lives in that map's worker and holds the physical generations and companion roles admitted for its stable key since it started, and `code.ingest` writes an unregistered default target (`<repository>/.khive/code-map.db`, `khive-pack-code/src/db_target.rs`) whether or not a registration names it.
  - _Exclusions._ These routes are outside the contract. Each is shown at source in the bundled SQLite; none is closed by the refusals above.
    - (a) A map or a map companion placed, through a hard link or another name, where an ordinary database's rollback journal, WAL or shared-memory file is derived. A parent's first write in a transaction opens `<database>-journal` for writing without an exclusive-create flag and writes a journal header into it, with no check of what that file is (`sqlite3.c:65456–65494`).
    - (b) Stock SQLite opening a map under any name, which acts on the derived companion names before it compares the header magic (`sqlite3.c:76450`): it checks for a hot journal and plays it back inside its shared-lock step (`sqlite3.c:64886–64895`, playback at `:64961`), opens an existing `-wal` as a WAL database whatever the header says (`pagerOpenWalIfPresent`, `:62944–62965`), and maps the WAL index through the main file's handle (`:68226`).
    - (c) `SQLITE_DBCONFIG_RESET_DATABASE` followed by `VACUUM`, which skips the magic check by design (`sqlite3.c:2776–2798`, `:76439–76441`).
    - (d) A checkpoint run through a second name and stopped part way. SQLite copies WAL frames into the main file one page at a time and records completion only after the loop (`sqlite3.c:69738–69774`), so the main file can depend on frames left in a WAL under a name the worker never sees. Removing that name does not repair the map, and the transition's acceptance of a `(-1, -1)` checkpoint result when it saw no WAL (`transition.rs:302–303`) shows neither that no such WAL exists nor that one was complete.
    - (e) A map with no application ID, reached by a path that is neither registered nor of the default-target shape: a map ingested before this change, until the worker next ingests it; an explicit `db` target between its creation by the operator and its first ingest (`code.ingest` requires an explicit target to exist already, `khive-pack-code/src/db_target.rs:297–305`); or a map the worker has created, at any target, before its first transaction commits. A parent pool pointed at such a file opens it as an ordinary database and writes it: its identity bootstrap, and WAL journal mode when the pool is configured for it (`construction.rs:202–245`).
    - (f) On Windows, a second name through a symbolic link or a directory junction under which SQLite derives companion names that differ from the map's. This is not verified at this revision, in either direction.
    - (g) Any other process with the same filesystem authority, including tools outside khive.
    - (h) A file placed at a configured backend's path after an `ATTACH` statement naming that path is prepared and before it executes. The authorizer's comparison has already passed, and `ATTACH` opens whatever is then at the path through the default VFS and reads its schema, which can play back a hot journal as in (b), before the post-attach binding refuses it (`sqlite3.c:125700`, `:125752`).

    The worker's refusals limit what follows one of these (a map with a second name is refused at its next open; a companion beside a rollback header is handled under W6), but the map may already have been changed. The remedy for a map changed or refused this way is to stop the process that opened it, remove the map, its companion files and any second name, and ingest again; an explicit target is first created again as an empty file, and a registered map is registered again only by restarting the parent, because registration records the file's identity (`khive-runtime/src/code_map_runtime.rs:376`), every read compares it (`:414–424`), and the runtime loads registrations once, at construction (`khive-runtime/src/runtime.rs:697`). Those are the current in-process sites; under W1 the worker obtains and compares the guarded identity for the parent, while the parent retains the registration metadata for the same lifetime. Removing only the second name is not a repair, per (d).
- **W3 Guarded admission for every worker open that can name a map.** Closing a descriptor on an inode whose locks the same worker holds can release that worker's locks. An alias of map B opened in map A's worker cannot release locks held only in B's separate worker; this narrows the lock hazard, not the admission or protected-file contract. Map B's operations still belong in B's worker, and a raw open is not authority to use or change B. For each worker:
  - Caller-named raw inputs are read in the parent and passed to the worker as data: source and manifest bytes, findings JSON, audit policy, executable digests, knowledge and blob inputs. An operation whose reads depend on map state, such as ingest deciding which files to parse again, asks the parent for each file's bytes through the operation protocol (W4). The paths an operation names are the paths its request names explicitly and the files the parent's walk of the operation's root found before the request. The parent serves bytes only from those paths, and only through the contained open the source readers use at this revision, which opens a regular file within the operation's canonical root and refuses anything else (`khive-pack-code/src/safe_source.rs:20–26`); a worker request cannot widen that set.
  - Every worker open of a path that can resolve to a governed map inode reads identity from the opened descriptor. A descriptor on an inode the worker already holds closes through the native VFS's deferral, which keeps it open while that inode has a live lock in the worker's lock table and closes it once none remains (`close_unlocked`, `khive-db/src/code_map_vfs/unix.rs:538–550`). An inspection descriptor whose opened identity is safe with respect to the protected set closes through that deferral and does not consume protected-alias quarantine. This is distinct from a protected-identity refusal during native admission: if the opened descriptor matches a protected member, or its post-open protected-identity validation cannot safely exclude that alias, retain that exact descriptor in Amendment 12's bounded quarantine before any read or use. In particular, the native main-header open in `prepare_rollback_target` is `transition_guard.open(Role::Main, ReadOnly)` (`transition.rs:201`), a guarded main-file admission even though its intended next action is a header read; a post-preflight protected-alias swap there quarantines the newly opened main descriptor, not a separate inspection copy or directory handle. These rules cover a caller-named target before its identity is known, a registered map's preflight and per-read revalidation, header and schema inspection, replacement verification and snapshot copies. The raw open, read and close helpers used for these at the census revision (`open_identity_file`, `sqlite_header_uses_wal`, `std::fs::copy`, default-VFS read-only opens) do not run in the worker on such paths.
  - Runtime files the worker uses (the volume-lease lock file, the database garbage-collection lock file, snapshot destinations) are created by the worker or checked on the opened descriptor against the identities the worker holds before use; a match refuses, and the refused descriptor closes through the same deferral. At the census revision the backend's garbage-collection owner opens `<db>.gc.lock` before schema preparation (`khive-db/src/backend.rs:655`), and pool construction opens the volume-lease lock file only when it initializes a new database (`khive-db/src/pool/construction.rs:202–208`); write leases open it again.
  - These rules are the Unix requirement. Windows byte-range locks belong to the handle that took them, so another handle's close releases nothing. The native identity-handle mechanism used on Windows for production-set sampling, the code-target equality proof and registered-map preflight (the last opens a read handle, `khive-runtime/src/code_map_runtime.rs:521–524`) may stay as it is. Its process ownership still follows W1/W2: registered-map preflight and per-read identity checks move into the map worker; only the explicit W2 parent-inspection exceptions stay in the parent.
- **W4 Operation protocol, no descriptor export.** On Unix the worker's socket is created mode `0600`, and each endpoint checks the peer uid. On Windows the worker's named pipe is created as the first instance of its name, with a security descriptor that grants access only to the user the parent runs as, and each endpoint checks the peer process the pipe reports (`GetNamedPipeClientProcessId` in the worker, `GetNamedPipeServerProcessId` in the parent) against the process it expects: the parent that started the worker, or the worker that parent started. Each endpoint completes its check before it sends or reads any operation; failure refuses the connection. The existing daemon socket's owner-only mode (`crates/khive-runtime/src/daemon/boot_guard.rs:217–237`) and the supervisor handover probe's same-uid check (`crates/khive-mcp/src/daemon/liveness.rs:450–462`) are precedents, not an assertion that a mutually checked worker channel already exists. Results cross the process boundary as owned values: rows, serialized snapshots, diagnostic counters. No file descriptor or HANDLE leaves the worker. The closure-based registered-map read API becomes an operation contract. For each operation the protocol states a bound on message size, with larger results streamed; cancellation, where a cancelled read rolls back its transaction and a cancelled write reports, for each transaction it ran, whether that transaction committed; and the outcome when the worker dies or the connection drops mid-operation. A read then fails with a typed retryable error. A write whose last transaction may have committed returns outcome-unknown. A single-transaction write is resolved by reading the map's state through the restarted worker, never by retrying blind. Ingest is not a request-wide transaction (Amendment 14): the earlier transactions of an interrupted ingest stay committed, as they do when the daemon stops mid-ingest at this revision, and the resolution is the same, running the ingest again through the restarted worker, including after a stop inside the prior-WAL transition (W6). Ingest is a multi-step exchange in which the worker requests file bytes (W3).
- **W5 No default-VFS peer inside the worker.** The worker opens governed maps only through the native VFS, so SQLite's default-VFS descriptor reuse has no peer to reuse from on a map inode. Test fixtures that seed a map with the default VFS do so before the worker opens it, or in a separate process that exits.
- **W6 Lifetimes.** Amendment 12's process-lifetime quarantine, its VFS registration table, the Unix guarded-identity ledger with its retained anchor descriptors (one per distinct map inode and role, retired only after the inode is unlinked and its last guarded open has closed) and the Windows guarded file-ID ledger are the worker's. At the census revision the Windows ledger appends one record per guarded open and never removes one (`khive-db/src/code_map_vfs/ledger.rs:66–70`, `83–90`). Recording each (file ID, role) once does not bound it: in rollback DELETE mode each write transaction creates and deletes a journal, and each new journal has a new file ID. So on both platforms an entry retires when the guarded VFS itself deletes that file, which is how SQLite removes a journal; on Unix an entry also retires as it does today, once the file is unlinked and its last guarded open has closed (`ledger.rs:108–117`). Both ledgers then hold the live files and retained older physical generations belonging to that worker's stable map key, with their companion roles. Each ledger has an effective charged-slot cap **L**. On Unix, sample the worker's soft `RLIMIT_NOFILE` at startup: for a finite limit, **L = min(4,096, floor(soft limit / 2))**; for an infinite limit, **L = 4,096**. Refuse startup with a typed resource error if L is below 6, before map admission. Six slots fit one admitted main generation and one replacement generation, each with its two companion reservations, without a refused charge or ledger-triggered draining. Successful occupancy equal to L does not latch draining; a refused next charge does, as specified below. This leaves at least half of a finite descriptor allowance outside ledger charges; it does not claim that allowance bounds every other descriptor class. Windows uses **L = 4,096**, with actual handle headroom and typed allocation failure qualified below. A retained main-file generation charges three slots: its own entry and one reserved slot each for a journal and transition WAL. Materializing one of those reserved companion entries does not charge it twice. A retained companion outside that reservation charges another slot. On Unix each materialized entry can also hold one anchor descriptor; unused reservations are capacity charges, not fictitious open descriptors. Check the charge before a main-file open that would add an entry, so, while its reservation holds, companion opens of an admitted map do not meet the ledger bound. Do not retire a linked older generation merely to fit another generation or an acceptance ramp: Amendment 12's retained-pin retirement condition still applies. The numerical ledger cap is not a claim of sufficient OS descriptor capacity. Actual descriptor or handle allocation failure remains a typed resource refusal; it neither authorizes an unguarded retry nor proves the registration-cap arm. The registration-cap fixture below supplies and measures its own sufficient descriptor headroom, rather than imposing a universal 8,192-descriptor startup minimum on ordinary operation. The reservation holds only while each companion entry retires when the guarded VFS deletes that file. A companion closed without that delete, because it was renamed away or its deletion failed, stays in the ledger while the file exists, and the next companion of the same main file then needs an entry the reservation does not cover; that open is refused at the cap like any other. The cap bounds ledger entries and their anchor descriptors. It does not count descriptors the native VFS's close deferral keeps open while a lock remains (`khive-db/src/code_map_vfs/unix.rs:518–550`); those close when the lock is released. **Known limit:** repeated inspection or refused-alias opens while such a lock remains can grow this deferred-descriptor set outside the quarantine, registration and ledger caps. This amendment records that existing unbounded count; it does not certify a total per-worker descriptor bound, and the resource fixtures must record this count separately. A main-file open adds no entry when, before the open, its name resolves through no-follow metadata on the pinned parent to an identity the ledger already holds as a main file. That open proceeds at the cap without creating the file, and the identity read from the opened descriptor must equal the one resolved before it; otherwise the open is refused as one that would add an entry. An identity the ledger holds as a main file counts as a reopen whichever name reaches it, because the worker's locks belong to the inode. An identity the ledger holds in another role, or one that names a protected file, is refused as at the census revision (cross-role alias `khive-db/src/code_map_vfs/mod.rs:744–750`; protected-alias quarantine `mod.rs:668`, `:700`). Every refused descriptor that is not kept in the protected-alias quarantine closes through the same deferral as W3's inspection opens, as such Unix refusals do at the census revision (`close_unlocked`, `mod.rs:736–747`); a refusal that may alias a protected file keeps its descriptor in that quarantine, as it does at the census revision (`mod.rs:697–724`), the early role refusals at `mod.rs:680–687` hold none, and the Windows delete path has no POSIX lock to release, so a refusal never releases a lock the worker holds on that inode. This lets an already admitted operation settle through its pool, replacement reader, per-request ingest runtime and prior-WAL transition, which opens the main file three times (`khive-db/src/code_map_vfs/transition.rs:201`, `:251`, `:368`), without an extra main-entry charge. At the cap every other main-file open is refused before the file is opened, as a request error naming the cap. This reopening permission is for settlement of already admitted work; it is not permission to admit new operations after cap-triggered draining begins below. Restarting the affected map's worker empties its two ledgers, as a daemon restart empties them under Amendment 12, and each identity is recorded again at its next guarded open. Until then the ledger holds no role for a closed physical generation. For one stable key A, after its worker exits, an old closed A main can be renamed onto A's derived companion name and a different main installed at A's unchanged pathname; A's successor has no old role record. The same absence of historical role evidence applies to an object imported from a different map worker. This gap follows every worker exit and restart, including automatic retirement after 60 idle seconds; it is not specific to a shared multi-map ledger. **Explicit limitation of this Proposed revision:** the Windows role-collision remedy below also automatically drains and recycles the worker, so a cross-role refusal can be only a one-request delay. The successor carries no refused file identity or old role fact through the parent. If the same physical object then appears as a valid main file and passes the other admission checks, the successor can admit it; the original role refusal is not a persistent prohibition. This revision explicitly retains that gap alongside the idle-retirement gap rather than claiming that companion-content refusal closes a main-role alias. Content refusal closes the database-as-companion case for every companion role the worker can destructively handle: refuse journal-role and transition-WAL-role opens of an existing admitted file whose first 16 bytes are exactly the SQLite database header string (`SQLite format 3` followed by a NUL byte), and refuse every journal, WAL or SHM destructive removal of such a file. Read the content from the exact safely admitted object, retain Amendment 12's no-follow, protected-identity and pinned-parent checks and revalidate the same identity before removal; all inspection/refusal closes obey W3. Check SHM content before its destructive lock/removal sequence. A steady-state SHM open is already forbidden by Amendment 12, and remains so. SQLite deletes the journal of a zero-page main file without opening it (`sqlite3.c:64783–64789`), so an open-time check alone does not reach that case. The same applies to recovery paths that would unlink a WAL with no commit frame. These content checks do not replace the protected-set checks or close #3593's exact production-handle gap; a database renamed to another main-file role is not a companion-content refusal. Without that check, a map moved onto a journal name reads as a hot journal to SQLite because its first byte is nonzero; its header fails the journal magic check, playback ends, and SQLite finalizes the journal, which in rollback DELETE mode deletes the map (`sqlite3.c:61217–61218`, `:62461–62466`, `:62588`). SQLite writes the first eight bytes of a journal only as the journal magic (`:60362`) or as zeros, so a real journal never starts with the database header string, a torn write included: zeros stand in the magic's place until the journal's first sync (`:61095`; the magic is written after that sync, `:63963–63968`); a journal finalized in place, in persist mode or in exclusive locking mode, has a zeroed header (`zeroJournalHdr`, `:61004–61005`, called at `:61695–61698`); and a journal has no bytes at all between its creation and its first header write (`:65476–65478`, `:65494`), after a finalization that truncates it, which takes a zero journal size limit, a super-journal or a temporary file (`:61001–61002`, `:61698`), and after each commit in truncate mode (`:61680–61684`). A worker that stops inside the prior-WAL transition, after `PRAGMA journal_mode=DELETE` succeeded and before the transition removed the remaining companion names (`transition.rs:331–346`), leaves a main file with a rollback header. Beside it there is always the `-wal`, and there can be a `-shm` from the map's WAL past, which the transition never creates or removes through SQLite because it uses an in-memory WAL index. The `-wal` remains because, during the transition, the guarded VFS answers SQLite's persist-WAL query for the main file with 1 (`khive-db/src/code_map_vfs/callbacks.rs:412–424`). SQLite's last WAL close therefore checkpoints and keeps the file, neither deleting nor truncating it, since at the census revision the transition's connection has no journal size limit: it is opened directly through the guarded VFS (`transition.rs:251–257`, `:94–102`), never through the pool setup that sets a limit on WAL connections (`khive-db/src/pool/connection_setup.rs:319–320`), so it keeps the per-connection value SQLite initializes from the compiled default (`sqlite3.c:64671`), which is -1 in this build (`:16813–16814`; the repository's `LIBSQLITE3_FLAGS` value, `.cargo/config.toml:20`, does not define it). The close's checkpoint (`:69970`) is then followed by neither branch that would remove or shorten the file: deletion is skipped because the `-wal` is persisted (`:69978–69982`), and truncation is skipped because the limit is -1 (`:69983–69990`); the transition removes it only in its cleanup (`transition.rs:331–346`). Under this amendment the transition sets `PRAGMA journal_size_limit=-1` on its connection before the FULL checkpoint (`sqlite3.c:67078–67081`), so the kept length no longer rests on how SQLite was compiled; a build that overrides `LIBSQLITE3_FLAGS` cannot change it. The frames were checkpointed into the main file (`transition.rs:292`). The rollback header SQLite writes after the close then changes page 1 in four fields: the format bytes 18–19, and the change counter, version-valid-for and version number at bytes 24–27 and 92–99 (`sqlite3.c:104589`, `:104604`; `:62689–62702`). At the census revision the transition refuses a rollback header with either companion and offers no way out (`transition.rs:219–228`), so running the ingest again, as W4 prescribes, would not resolve it. The worker's transition recovers this state under its own conditions. No other code-map connection holds the map. The exclusive lock is taken in rollback mode with no busy wait, and no WAL-mode connection can share it, because a WAL-mode connection keeps its shared lock on the main file for as long as it is open (`sqlite3.c:61457–61470`, `:60743`). Every remaining companion passes the same no-follow admission as a WAL transition's. The worker never replays the `-wal`. Stock SQLite opens an existing `-wal` as a WAL database whatever the header says (`pagerOpenWalIfPresent`, `sqlite3.c:62944–62965`). So before removing it, the worker reads the state SQLite would recover from it, every page up to its last valid commit frame and the database size that commit records (`walIndexRecover`, `:68826`; the size is the commit frame's `nTruncate`, `:68954`), and compares the size with the main file's length in pages and each page with the main file's, page 1 outside the four fields above. Only after the database-header content refusal above has passed may a `-wal` with no valid commit frame, an empty one included, be treated as recovering to the main file unchanged: only a commit frame advances the recovered state (`:68857`, `:68952–68954`). Such a WAL adds nothing and may be unlinked as below. A database file masquerading as a WAL also lacks a valid commit frame; that fact never authorizes deleting it. For the transition's own `-wal`, when it holds a commit frame, the two sizes are equal. The transition switches modes only after a FULL checkpoint that returns busy 0 and has backfilled every frame of the log (`transition.rs:291–311`); after that backfill the checkpoint truncated the main file to the page count the wal-index header holds for the last commit, recovered from that commit frame's `nTruncate` (`sqlite3.c:69764–69769`, `:68954`); no statement runs on the connection between that checkpoint and the switch (`transition.rs:312–333`); and no other connection writes a frame in between, because from its `BEGIN EXCLUSIVE` (`transition.rs:273`), which runs before the checkpoint, the transition's connection holds the exclusive lock of exclusive locking mode (`:265–278`, proved again at `:321–324`), and that lock refuses the reads and writes of every client that honours SQLite's locks; a process that bypasses them, by ignoring the lock, opening with `nolock` or reading with `immutable=1`, is exclusion (g). So the close inside the switch (`sqlite3.c:104589–104592`) finds every frame backfilled, its checkpoint copies nothing (`:69970–69971`, `:69658`), and it keeps the `-wal` at its length (`:69974–69990`): it changes neither size. When every page is equal, the `-wal` adds nothing to the main file, and it is unlinked through the pinned parent. When any page differs, the map is refused with a typed error naming both files, with the remedy W2 names: a `-wal` that differs was written by another client or placed there. A `-shm` is unlinked only after its database-header content refusal passes and the worker takes its dead-man-switch lock exclusively, refusing if another process holds it (Unix: the byte SQLite's shared-memory open locks, `sqlite3.c:44790`, `:45037–45080`); on Windows a failed removal refuses. The transition then runs the rollback reopen proof (`transition.rs:362–396`). A companion that fails admission is refused as at the census revision, with the remedy W2 names. Registration churn depends on the registered operation. At the current-base revision, ordinary rollback-writer registrations can be reused after a DELETE-mode target replacement because their cache key has no expected main-file identity; that replacement alone does not prove registration growth. A fresh quiescent prior-WAL transition registers its own guarded VFS before the rollback-writer registration and consumes a new slot. The replacement-churn acceptance uses that actual transition route and records each increment under one stable map key. Once map workers exist, each cap has an explicit draining trigger. For quarantine, retaining a refused descriptor that brings either its domain occupancy to 8 or the worker-wide occupancy to 32 latches draining; a reservation refused because it would exceed either quarantine cap also latches it without opening an unretainable descriptor. For registrations, filling the 512th slot succeeds without latching; a subsequent request that needs a new registration and is refused because it would exceed 512 latches draining. Reusing a registration at occupancy 512 does not latch it. For ledger charges, a successful reservation that brings occupancy exactly to L does not latch; a reservation refused because its next charge would exceed L does. A successful reopen or reserved-companion open adds no charge and does not latch merely because occupancy equals L. On any of those triggers the affected worker latches a draining state: it admits no new operations, returns a typed cap-and-recycling error to requests that cannot be admitted, settles already admitted work and pending replies under W4, closes and exits without waiting for the idle interval. The parent reaps it before releasing or replacing its key reservation; the next request starts a successor for that key within its existing deadline. This is the concrete automatic restart remedy, not an unspecified operator verb. A failed reservation does not increment or round up the resource count: with L=4,096, a ledger charge of 4,095 followed by a three-slot request refuses 4,098, retains charge 4,095 and still latches draining. Cap state cannot be kept alive by a stream of new requests. Already admitted operations still obey every guarded-open cap; permission to settle is no cap bypass. An unresponsive drain follows W7, including outcome-unknown for an uncertain write. A Windows role-collision refusal uses the same affected-worker drain-and-recycle mechanism, with the immediate loss of historical role evidence explicitly recorded above. This is a separate trigger from the three resource caps; it does not increment a cap count or claim that the colliding object was removed. Recycling reclaims only that worker's resource state; other map workers and their operations continue. Worker restart is a worker-era remedy, not an available action in the currently shipped in-process implementation. It does not refresh the parent's registered file identity: W2's parent-restart requirement for re-registration still applies. Inventory the parent's descriptors across a restart. Only the affected worker's control, lifetime and supervision endpoints may change. The parent holds no map data descriptor for the governed operations moved under W1; W2's explicit parent-opener exceptions remain unchanged. Every other parent descriptor, including existing exception-route descriptors and other workers' channels, remains unchanged. `db_diagnostics` reports the ledger's entry count beside the quarantine and registration counts, identifying the worker and map key to which each count belongs. Non-adversarial runs still end with quarantine occupancy zero.
- **W7 Failure.** If the addressed map's worker is absent and cannot be started, its request fails with a typed error naming the start failure. A crashed or unresponsive worker fails requests addressed to that worker, not requests to other map workers. W4's retryable read and outcome-unknown write rules remain; neither restart nor retirement converts an uncertain write into a known rollback. The parent never falls back to an in-process map open. Parent-capacity refusal, failed-first-admission cleanup, cap-triggered draining and idle retirement obey the reservation and lifetime rules above; no busy worker is evicted merely to admit a different key. An operation already in flight when its worker drains must settle within its existing operation deadline; if it cannot, supervision terminates and reaps that worker and applies W4's failure outcomes. A later request may wait for that reap within its own deadline, but cannot extend the draining worker's lifetime indefinitely. Other supervision choices remain implementation decisions, covered by acceptance.
- **W8 No lease held across a worker call.** A parent thread holding a volume lease, writer admission or garbage-collection lock never waits on the worker. Across processes a volume lease is taken by polling an advisory lock file with no fairness (`khive-db/src/disk_guard.rs`), so a parent that holds a same-volume lease while waiting on a worker that needs it stalls both until the lease deadline. Parent threads queue for the lease in arrival order inside their own process before polling the lock file, while as many as 16 map workers independently poll the same volume's lock file from outside that queue (`khive-db/src/disk_guard.rs:205–248`). The workers also contend with one another; per-map isolation supplies no cross-process fairness. Under sustained same-volume parent or sibling-worker writes, a worker can wait to its deadline even when no parent thread is waiting on it. That wait ends as a typed lease timeout, never an unbounded stall, and the Lease contention arm measures it. It is not permission to count a timeout as success in the #5614 availability arm: that arm places C on a separately identified volume, while a separate same-volume contention arm records the unfair lease boundary without claiming availability.

### Census and dispositions

Source census at `f9ebcf467e20c9312f8b70c26897191cbedf5a90` (paths under `crates/` unless named otherwise). "Today" describes that revision. "Guarded" means native opened-handle proof, not whole-process lock safety; "unguarded" means the opener can use an ordinary or raw descriptor on a map or alias, not that every existing check is absent. The census and the executable acceptance population must be the **same population**. Each row denotes the actual API or helper and all its callers; main files, companions, initial and later opens and same-inode aliases remain included. An exported function without a current caller stays in the population.

| Class                                                            | Entry point at the census revision                                                                                                                                                                                                                               | Today                                                                                                                                                                    | Disposition                                                                                                                                                                                                                                                                                                                                                                                                                                                          |
| ---------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Pool/runtime initial writer                                      | `khive-db/src/pool/construction.rs:154`; `khive-db/src/pool/claimed_file_identity.rs:43`; `khive-db/src/pool/code_map.rs:19`                                                                                                                                     | Guarded only when the pool carries the code-map VFS; ordinary construction on a map alias is unguarded                                                                   | Worker (W1); parent ordinary construction naming a map refuses before and after the open (W2)                                                                                                                                                                                                                                                                                                                                                                        |
| Pooled readers, initial and replacement                          | `khive-db/src/pool/connection_setup.rs:232`; `khive-db/src/pool/pool_impl.rs:622`, `:957`                                                                                                                                                                        | Same conditional; recycling uses the same funnel                                                                                                                         | Worker, every map reader including replacements                                                                                                                                                                                                                                                                                                                                                                                                                      |
| Standalone readers and cached read connections                   | `khive-db/src/pool/pool_impl.rs:887`                                                                                                                                                                                                                             | Same conditional                                                                                                                                                         | Worker                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| Standalone writers, writer task, checkpoint, diagnostics probe   | `khive-db/src/pool/pool_impl.rs:671`, `:698`                                                                                                                                                                                                                     | Guarded with the VFS, default VFS otherwise                                                                                                                              | Worker; the parent aggregates diagnostic values the worker returns                                                                                                                                                                                                                                                                                                                                                                                                   |
| Schema and snapshot inspection, raw header read                  | `khive-db/src/migrations.rs:1527`, `:1535`, `:1555`, `:1574`; `khive-db/src/pool/connection_setup.rs:159`, `:168`                                                                                                                                                | Raw `File::open`, read and close of the header, then a default-VFS open                                                                                                  | Worker through guarded admission (W3), preserving read-only and no-migration semantics; parent refuses a map target                                                                                                                                                                                                                                                                                                                                                  |
| Embedding-model registry read                                    | `khive-db/src/migrations.rs:2188` (`query_embedding_models`, exported, no production caller)                                                                                                                                                                     | Unguarded default-VFS open                                                                                                                                               | Worker through guarded admission for a map target, or typed refusal; ordinary databases stay in the parent                                                                                                                                                                                                                                                                                                                                                           |
| Showcase map read and export                                     | `khive-repo-showcase/src/read.rs:50`, `:253`                                                                                                                                                                                                                     | Unguarded default-VFS read-only open                                                                                                                                     | Worker reads, parent renders and writes the export                                                                                                                                                                                                                                                                                                                                                                                                                   |
| Configured backend construction (`[[backends]]`, B7's read path) | `khive-mcp/src/serve/storage_topology.rs:47`, `khive-mcp/src/serve/claimed_backend.rs:115`                                                                                                                                                                       | Opens every declared SQLite backend through the ordinary pool constructor, a declared map included                                                                       | Admitted-target constructor; a declared map the known-map refusals recognize is refused before SQLite opens it, and the configuration does not load (W2, configured map backends)                                                                                                                                                                                                                                                                                    |
| Writable SQL `ATTACH` (writer, scripts, raw handles)             | `khive-db/src/sql_bridge/sqlite_writer.rs:373`, `:519`                                                                                                                                                                                                           | No map-target attachment gate                                                                                                                                            | Refused in both processes before the file opens unless canonical path and file identity match a configured backend as the runtime constructed it, compared when the statement is prepared, admitted only through the bridge writer's single-statement route, which binds the attached file before the call returns, and refused on the script route and on raw handles; the empty-name temporary attachment `VACUUM` makes also remains admitted (W2; exclusion (h)) |
| Reader SQL `ATTACH`                                              | `khive-db/src/sql_bridge/reader_capability.rs:244`                                                                                                                                                                                                               | Refused before execution                                                                                                                                                 | Refused in parent and worker                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| Dedicated ingest constructor and migrations                      | `khive-pack-code/src/handlers.rs:151`; `khive-runtime/src/code_map_runtime.rs:553`; `khive-db/src/backend/code_map.rs:77`                                                                                                                                        | Guarded; active since #4129. The GC owner opens `<db>.gc.lock` (`khive-db/src/backend.rs:655`); a new database also takes the volume lease at construction               | Worker: transition, pool, migration, ingest, rebase, shutdown; lock files under W3; source bytes from the parent (W3, W4)                                                                                                                                                                                                                                                                                                                                            |
| Native main and journal opens, hot-journal recovery              | `khive-db/src/code_map_vfs/vfs.rs:233`; `khive-db/src/code_map_vfs/mod.rs:518`, `:668`                                                                                                                                                                           | Guarded; a caller-named target is opened before its identity is read                                                                                                     | Worker, with no default or raw peer of the map in the worker (W3, W5)                                                                                                                                                                                                                                                                                                                                                                                                |
| Prior-WAL transition and rollback reopen proof                   | `khive-db/src/code_map_vfs/transition.rs:94`, `:146`, `:173`                                                                                                                                                                                                     | Guarded                                                                                                                                                                  | Worker, the whole transition and its post-reopen proof                                                                                                                                                                                                                                                                                                                                                                                                               |
| Protected-alias quarantine and cap                               | `khive-db/src/code_map_vfs/mod.rs:668`, `:700`                                                                                                                                                                                                                   | Retained for the process lifetime; at the cap, refusal before another open                                                                                               | Worker; worker restart reclaims (W6)                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| Guarded identity ledger                                          | `khive-db/src/code_map_vfs/ledger.rs:43`, `:110`                                                                                                                                                                                                                 | Unix keeps one anchor descriptor per map inode and role, retired at link count zero with no guarded open, no cap; Windows keeps file-ID records for the process lifetime | Worker; retired on delete, capped, counted; restart reclaims; no descriptor or HANDLE exported (W4, W6)                                                                                                                                                                                                                                                                                                                                                              |
| VFS registration reuse                                           | `khive-db/src/code_map_vfs/vfs.rs:53`                                                                                                                                                                                                                            | Reused only for a matching target, protected contract and identity, so each map replacement takes a new slot                                                             | Worker-local table and cap; restart reclaims (W6)                                                                                                                                                                                                                                                                                                                                                                                                                    |
| Registered-map identity preflight                                | `khive-runtime/src/code_map_runtime.rs:136`, `:318`, `:508`                                                                                                                                                                                                      | Raw no-follow open, check, close, before any guarded open                                                                                                                | Worker performs initial guarded identity/header admission during parent runtime construction and returns owned identity metadata; the parent parses and retains registration metadata without a map descriptor (W1, W3)                                                                                                                                                                                                                                              |
| Registered-map per-read revalidation                             | `khive-runtime/src/code_map_runtime.rs:221`, `:399`, `:508`                                                                                                                                                                                                      | Raw open and close of the header before the guarded read                                                                                                                 | Worker, revalidating on the retained descriptor in place of the separate raw open and close (W3)                                                                                                                                                                                                                                                                                                                                                                     |
| Registered-map read transaction                                  | `khive-runtime/src/code_map_runtime.rs:238`; `khive-db/src/backend/code_map.rs:22`                                                                                                                                                                               | Guarded read-only transaction                                                                                                                                            | Worker; owned values over the boundary (W4)                                                                                                                                                                                                                                                                                                                                                                                                                          |
| Genuine governed integration test                                | `khive-pack-code/tests/reserved_properties.rs:83`                                                                                                                                                                                                                | Calls `KhiveRuntime::new_code_map`, the governed route                                                                                                                   | Worker, with the same protected-owner evidence as production                                                                                                                                                                                                                                                                                                                                                                                                         |
| Replacement verification                                         | `khive-vcs/src/sync.rs:1372`                                                                                                                                                                                                                                     | Unguarded default-VFS read-only open                                                                                                                                     | Worker through guarded admission for a governed map; ordinary store sync stays in the parent                                                                                                                                                                                                                                                                                                                                                                         |
| CLI findings and backfill snapshot copy                          | `kkernel/src/code_ingest.rs:748`                                                                                                                                                                                                                                 | Unguarded `fs::copy` of main, WAL and SHM                                                                                                                                | Worker copies through its guarded connection or retained descriptor into a destination it creates (W3); the parent receives a completed copy whose identity differs from the map                                                                                                                                                                                                                                                                                     |
| SQLite descriptor reuse                                          | `Cargo.lock:3241` (libsqlite3-sys 0.38.1); `sqlite3.c:46556`, `:46818`, `robust_open` `:46868`, inode list `:41536` (bundled); rusqlite 0.40.1 `src/inner_connection.rs:90`                                                                                      | Default `unixOpen` can reuse a descriptor without an OS open                                                                                                             | No default-VFS map open in the worker (W5); refused in the parent                                                                                                                                                                                                                                                                                                                                                                                                    |
| Source and manifest ingestion                                    | `khive-pack-code/src/safe_source.rs:20`, `:55`; `khive-pack-code/src/source_ingest.rs:1866`; `khive-fs/src/opened_file.rs:93`                                                                                                                                    | Raw open with containment and size checks; no map-identity check, so a hard link to a map is readable                                                                    | Parent reads and passes bytes (W3, W4)                                                                                                                                                                                                                                                                                                                                                                                                                               |
| Executable digest                                                | `khive-pack-exec/src/handlers.rs:708`, raw open `:726–734`                                                                                                                                                                                                       | Raw no-follow read, regular-file check                                                                                                                                   | Parent                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| Captured command output                                          | `khive-pack-exec/src/capture.rs:295`, `:363`, directory root `:165–168`; `khive-fs/src/fd_relative.rs:117`                                                                                                                                                       | Raw leaf descriptor                                                                                                                                                      | Parent                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| Knowledge import and evaluation query set                        | `khive-pack-knowledge/src/knowledge/sections.rs:520`; `khive-pack-knowledge/src/knowledge/eval.rs:138`; shared no-follow helper `khive-fs/src/opened_file.rs:40–52`                                                                                              | Raw reads                                                                                                                                                                | Parent                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| Blob import, shard read, touch and sweep                         | `khive-pack-blob/src/file_handlers.rs:46`; `khive-runtime/src/file_policy.rs:173`; `khive-db/src/stores/blob/root_handle.rs:443–490`, `:494`; `khive-db/src/stores/blob/publish.rs:3–34`, `:487`                                                                 | Raw leaf and directory opens                                                                                                                                             | Parent                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| Git cache staging locks and Windows leaf pins                    | `khive-pack-git/src/cache.rs:663`, `:1685`, directory opens `:751–755`; `khive-pack-git/src/cache_windows.rs:167`                                                                                                                                                | Raw write and pin opens; no map identity checked                                                                                                                         | Parent                                                                                                                                                                                                                                                                                                                                                                                                                                                               |
| Code-target identity preflight on Windows                        | `khive-pack-code/src/db_target.rs:102`, through `same-file` 1.0.6 `src/lib.rs:370–375`, `src/win.rs:110–114`                                                                                                                                                     | Windows opens read handles to compare identity; Unix compares metadata only                                                                                              | Parent under W2's explicit Windows code-target equality preflight exception; Windows locks are per handle; the worker independently reads the opened target's identity on guarded admission (W3)                                                                                                                                                                                                                                                                     |
| CLI findings JSON and audit policy bytes                         | `kkernel/src/code_ingest.rs:202`; `kkernel/src/code_audit.rs:122`                                                                                                                                                                                                | Raw reads before runtime resolution                                                                                                                                      | Parent, before the worker is asked                                                                                                                                                                                                                                                                                                                                                                                                                                   |
| Repository bundle export output                                  | `kkernel/src/repo.rs:459`; `khive-repo-showcase/src/export.rs:137`, `:215`                                                                                                                                                                                       | New temporary output file; map reads as the showcase row                                                                                                                 | Parent output; map reads in the worker                                                                                                                                                                                                                                                                                                                                                                                                                               |
| Default-VFS test seeding, lock observers, expected-failure opens | `khive-db/src/code_map_vfs/tests.rs:23`, `:55`, `:149`, `:254`, `:441`, `:573`, `:815`; `khive-db/src/code_map_vfs/sidecar_barrier_tests.rs:44`, `:246`                                                                                                          | Default VFS and raw alias swaps by design; observers run as child processes                                                                                              | Seed before the worker opens or in a process that exits; observers stay separate processes, never the process holding the tested lock                                                                                                                                                                                                                                                                                                                                |
| Generic-runtime test helper                                      | `khive-pack-code/src/source_ingest/owner_alias_tests.rs:7`                                                                                                                                                                                                       | `KhiveRuntime::new`, the ordinary backend                                                                                                                                | Test-only ordinary fixture; governed behaviour is tested through the worker                                                                                                                                                                                                                                                                                                                                                                                          |
| Python integration SQLite reads                                  | `python/tests/test_comm_idempotency.py:44–51`                                                                                                                                                                                                                    | Read-only URI on a scratch daemon database, opened by the test process                                                                                                   | Keep the separate test process; read-only flags alone do not make a same-process close safe                                                                                                                                                                                                                                                                                                                                                                          |
| Python benchmark SQLite, SQL lint                                | `benches/retrieval/evaluate.py:699`, `:720`; `scripts/lint-sql.sh:44`                                                                                                                                                                                            | Separate processes on scratch databases or `:memory:`                                                                                                                    | Unchanged; outside the map population                                                                                                                                                                                                                                                                                                                                                                                                                                |
| Workspace pack                                                   | `khive-pack-workspace/src/pack.rs:26`, `:33–35`, `:70–81`; `khive-pack-workspace/src/hook.rs:17–55`; metadata macros in `khive-runtime/src/pack_metadata.rs:24–98`; fixtures `khive-pack-workspace/tests/integration.rs:15–17`, `tests/proposal_entity.rs:16–27` | No opener of its own                                                                                                                                                     | Parent                                                                                                                                                                                                                                                                                                                                                                                                                                                               |

Caller-input raw readers in the parent cannot release the worker's locks, even through an alias of the same inode, so those census rows stay in the parent. The map snapshot copy is the explicit exception: it is map-state access under W1, and its raw main/WAL/SHM copies run before the read-only constructor (`crates/kkernel/src/code_ingest.rs:762–802` at the new source revision), so dispatch must happen before the first copy rather than at that later constructor. Reading a map's bytes through a link is a question about each reader's input, not a lock hazard, and this amendment adds no rule for it. Their hazard against the parent's own production connections is the existing one that Amendment 9's deny set and #3593 address, unchanged here.

**Residual categories, open acceptance obligations.** The census is a pinned inventory plus a whole-tree lexical pass, not an exhaustive reachability proof. These remain open: imported names, aliases, traits and wrappers without a cross-crate symbol-resolution pass; macro expansions and generated code; transitive dependencies and platform branches (the Windows layer below `same-file` was not read); backup, export and restore aliases and dynamically assembled SQL; subprocesses and dynamically selected tools; test fixtures not individually traced; raw file aliases inside the worker (W3 states the rule; only executed acceptance shows it holds); and any source change after the census revision, which needs a delta census at the implementation's base.

#### Delta census at cf9ceab6

This supplements the census at `f9ebcf467e20c9312f8b70c26897191cbedf5a90` with source at
`cf9ceab64a1854ad0e2d7540f970b1eb289465b1`. The historical table and its dispositions remain
in place. All **37 historical rows are retained**; the twelve groups below record
additional or changed opening paths, including existing paths omitted from that
table. These groups are not twelve wholly new file-opening classes: one is
in-memory only, and others are moved helpers or changed ownership of existing
opens. **No opening class in the reconciled population was removed.** Removal of
an API name is not removal of its consumers or descriptor lifetimes.

The population rule is unchanged: actual APIs/helpers and their callers,
exported APIs without current callers, main files and companions, initial and
later connections, and same-inode aliases. The separate-process test and
outside-map rows remain explicitly classified. Paths in the tables are relative
to `crates/`, except `python/`, `benches/` and `scripts/`; every line number in this
subsection names the newer revision unless explicitly labelled historical.

**Current anchors for the retained population.** These short entries update
locations and source deltas; they do not replace the preceding descriptions or
supply executed evidence for their worker/refusal/parent dispositions.

| Retained class                                                   | Source delta                                                                                       | Current anchors                                                                                                                                                           |
| ---------------------------------------------------------------- | -------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Pool/runtime initial writer                                      | Live read-only construction added; initial-writer funnel retained.                                 | `khive-db/src/pool/construction.rs:181, :52`; `khive-runtime/src/runtime.rs:628`                                                                                          |
| Pooled readers, initial and replacement                          | Initial and replacement reader locations moved; same funnel.                                       | `khive-db/src/pool/connection_setup.rs:235`; `khive-db/src/pool/pool_impl.rs:638, :1076`                                                                                  |
| Standalone readers and cached read connections                   | Standalone-reader location moved; separate lifetime retained.                                      | `khive-db/src/pool/pool_impl.rs:1006`                                                                                                                                     |
| Standalone writers, writer task, checkpoint, diagnostics probe   | Raw writer factories replaced by complete-consumer factories and private opener; no class removed. | `khive-db/src/pool/pool_impl.rs:679, :709, :713, :721, :734, :745, :761, :785`                                                                                            |
| Schema and snapshot inspection, raw header read                  | Inspection locations moved; raw header and snapshot opens retained.                                | `khive-db/src/migrations.rs:1681`; `khive-db/src/pool/connection_setup.rs:159, :171`                                                                                      |
| Embedding-model registry read                                    | Exported default-VFS registry read retained, even without a located production caller.             | `khive-db/src/migrations.rs:2297`                                                                                                                                         |
| Showcase map read and export                                     | Listed read entry points unchanged.                                                                | `khive-repo-showcase/src/read.rs:50, :253`                                                                                                                                |
| Configured backend construction (`[[backends]]`, B7's read path) | Ordinary configured-backend construction retained.                                                 | `khive-mcp/src/serve/storage_topology.rs:47`; `khive-mcp/src/serve/claimed_backend.rs:115`                                                                                |
| Writable SQL `ATTACH` (writer, scripts, raw handles)             | Writer admission API changed; single-statement, script and raw-handle arms retained.               | `khive-db/src/sql_bridge/sqlite_writer.rs:369, :498`                                                                                                                      |
| Reader SQL `ATTACH`                                              | Reader attachment refusal retained.                                                                | `khive-db/src/sql_bridge/reader_capability.rs:244`                                                                                                                        |
| Dedicated ingest constructor and migrations                      | In-process guarded construction retained; constructor location moved.                              | `khive-pack-code/src/handlers.rs:151`; `khive-runtime/src/code_map_runtime.rs:576`; `khive-db/src/backend/code_map.rs:77`                                                 |
| Native main and journal opens, hot-journal recovery              | Native opens retained; Windows main-handle control added.                                          | `khive-db/src/code_map_vfs/vfs.rs:232`; `khive-db/src/code_map_vfs/mod.rs:679`; `khive-db/src/code_map_vfs/callbacks.rs:447, :458`                                        |
| Prior-WAL transition and rollback reopen proof                   | Transition source unchanged; all three opens retained.                                             | `khive-db/src/code_map_vfs/transition.rs:94, :146, :173`                                                                                                                  |
| Protected-alias quarantine and cap                               | Pre-open reservation added: eight per pinned-parent/leaf domain, process cap32.                    | `khive-db/src/code_map_vfs/mod.rs:32, :33, :855, :875`                                                                                                                    |
| Guarded identity ledger                                          | Ledger source unchanged.                                                                           | `khive-db/src/code_map_vfs/ledger.rs:43, :110`                                                                                                                            |
| VFS registration reuse                                           | Matching-registration reuse and process cap512 retained.                                           | `khive-db/src/code_map_vfs/vfs.rs:53`                                                                                                                                     |
| Registered-map identity preflight                                | Raw preflight retained; opened-file link-count-one check added.                                    | `khive-runtime/src/code_map_runtime.rs:333, :531, :396`                                                                                                                   |
| Registered-map per-read revalidation                             | Raw revalidation retained; link-count-one check added.                                             | `khive-runtime/src/code_map_runtime.rs:234, :531, :447`                                                                                                                   |
| Registered-map read transaction                                  | Read transaction location moved; closure-based in-process API retained.                            | `khive-runtime/src/code_map_runtime.rs:251`; `khive-db/src/backend/code_map.rs:22`                                                                                        |
| Genuine governed integration test                                | Governed fixture unchanged.                                                                        | `khive-pack-code/tests/reserved_properties.rs:83`                                                                                                                         |
| Replacement verification                                         | Verification location moved; ordinary SQLite read retained.                                        | `khive-vcs/src/sync.rs:1498`                                                                                                                                              |
| CLI findings and backfill snapshot copy                          | Snapshot location moved; main/WAL/SHM copies still precede backend construction.                   | `kkernel/src/code_ingest.rs:736`                                                                                                                                          |
| SQLite descriptor reuse                                          | Locked SQLite dependencies changed; default Unix descriptor reuse retained.                        | `Cargo.lock:3543`                                                                                                                                                         |
| Source and manifest ingestion                                    | Shared no-follow helper extracted; source-reader role retained.                                    | `khive-pack-code/src/safe_source.rs:20, :55`; `khive-pack-code/src/source_ingest.rs:1866`; `khive-fs/src/opened_file.rs:102`                                              |
| Executable digest                                                | Unix digest open uses shared no-follow helper; non-Unix open retained.                             | `khive-pack-exec/src/handlers.rs:712, :728`; `khive-fs/src/opened_file.rs:39`                                                                                             |
| Captured command output                                          | Capture opens retained; bounded readlink helper extracted.                                         | `khive-pack-exec/src/capture.rs:300, :346, :170`; `khive-fs/src/fd_relative.rs:117`                                                                                       |
| Knowledge import and evaluation query set                        | Reader locations moved; raw input role retained.                                                   | `khive-pack-knowledge/src/knowledge/sections.rs:521`; `khive-pack-knowledge/src/knowledge/eval.rs:165`; `khive-fs/src/opened_file.rs:55`                                  |
| Blob import, shard read, touch and sweep                         | Listed import/root/publish sources unchanged.                                                      | `khive-pack-blob/src/file_handlers.rs:46`; `khive-db/src/stores/blob/root_handle.rs:443, :494`; `khive-db/src/stores/blob/publish.rs:487`                                 |
| Git cache staging locks and Windows leaf pins                    | Lock/directory locations moved; Windows pin helper unchanged.                                      | `khive-pack-git/src/cache.rs:678, :1700, :766`; `khive-pack-git/src/cache_windows.rs:167`                                                                                 |
| Code-target identity preflight on Windows                        | Preflight and same-file1.0.6 unchanged.                                                            | `khive-pack-code/src/db_target.rs:102`                                                                                                                                    |
| CLI findings JSON and audit policy bytes                         | Input-read location moved; reads still precede runtime resolution.                                 | `kkernel/src/code_ingest.rs:200`; `kkernel/src/code_audit.rs:122`                                                                                                         |
| Repository bundle export output                                  | Listed export/output sources unchanged.                                                            | `kkernel/src/repo.rs:459`; `khive-repo-showcase/src/export.rs:137, :215`                                                                                                  |
| Default-VFS test seeding, lock observers, expected-failure opens | Fixtures extended for quarantine budgets, concurrent opens and Windows handle control.             | `khive-db/src/code_map_vfs/tests.rs:193, :298`; `khive-db/src/code_map_vfs/sidecar_barrier_tests.rs:44`; `khive-db/src/code_map_vfs/quarantine_budget_tests.rs:189, :526` |
| Generic-runtime test helper                                      | Ordinary-runtime fixture unchanged.                                                                | `khive-pack-code/src/source_ingest/owner_alias_tests.rs:7`                                                                                                                |
| Python integration SQLite reads                                  | Separate-process fixture unchanged.                                                                | `python/tests/test_comm_idempotency.py:44`                                                                                                                                |
| Python benchmark SQLite, SQL lint                                | Benchmark unchanged; SQL-lint additions retain scratch/in-memory disposition.                      | `benches/retrieval/evaluate.py:699, :720`; `scripts/lint-sql.sh:44`                                                                                                       |
| Workspace pack                                                   | Zero public verbs retained; directory/input openers added by mirror and cursor.                    | `khive-pack-workspace/src/pack.rs:26`; `khive-pack-workspace/src/mirror.rs:374, :562, :564, :643`; `khive-pack-workspace/src/cursor.rs:150`                               |

The nine unchanged source/fixture entries above (showcase reads, prior-WAL
transition, identity ledger, governed integration fixture, listed blob paths,
Windows target preflight, export output, ordinary-runtime fixture and Python
integration fixture) keep their complete historical populations. Other rows
with retained behavior still have moved locations or changed surrounding code.
The removed `open_standalone_writer`,
`open_standalone_writer_for_admitted_operation` and
`open_standalone_writer_untracked` APIs are replaced by consumer-specific
factories using the private `open_standalone_connection`; standalone units,
checkpoints, writer tasks, SQL writers, diagnostics and FTS maintenance remain
in the acceptance population.

**Additional and moved paths.** Each group retains its actual callers and
separate input/output or initial/later opens. Parent raw opens remain distinct
from later SQLite construction.

| Group                                    | Delta and disposition                                                                                                                                                                                                               | Current anchors                                                                                                                                                                           |
| ---------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Events pruning SQLite connections        | Added direct default-VFS source readers, apply writer and backup-destination connection. Parent ordinary-database admission/refusal applies independently to each; schema checks after open do not establish pre-open refusal.      | `khive-db/src/events_prune.rs:282, :299, :348, :749`; `kkernel/src/events_prune.rs:155, :169, :191`                                                                                       |
| Events backup and execution records      | Added digest input, receipt, ledger reads and create/append outputs. Parent raw-input/output population; preserve independent open and close arms.                                                                                  | `khive-db/src/events_prune.rs:702, :813, :924, :1044, :1083`                                                                                                                              |
| Maintenance declaration and claims       | Added exported directory, record, claim and publication helpers, plus fixed process-identity reads. Parent control-file population; fixed `/proc` paths are not arbitrary map paths. Exported APIs remain included without callers. | `khive-db/src/maintenance/record_io.rs:80, :161, :311, :463`; `khive-db/src/maintenance/process.rs:23`                                                                                    |
| Atomic create-only output                | New exported staging/directory helper; existing CLI initialization callers migrated to it. Parent output, with no governed-map read implied.                                                                                        | `khive-fs/src/atomic_create.rs:76, :203, :371`; `kkernel/src/kg/init.rs:128, :235`                                                                                                        |
| Events-store file hardening              | Existing creation and main/WAL/SHM permission paths extracted from daemon code. Attested regular-file descriptors now perform permission updates. Parent file population is distinct from the later SQLite pool.                    | `khive-runtime/src/events_split/file_hardening.rs:90, :128, :139`; `khive-runtime/src/events_split/events_daemon.rs:613`; `khive-runtime/src/events_split/registry.rs:272`                |
| Daemon locks, PID and supervisor markers | Existing parent reads/opens moved to pinned-directory, no-follow and regular-file helpers with bounded PID reads. Directory and leaf descriptors, aliases and later closes remain separate obligations.                             | `khive-runtime/src/daemon/locks.rs:100, :214`; `khive-runtime/src/daemon/process_files.rs:205, :362`; `khive-runtime/src/daemon/run_daemon.rs:394`; `khive-mcp/src/daemon/forward.rs:389` |
| Vector checkpoint lock and segments      | Existing Unix raw opens migrated to shared `open_file_at`/`open_at`. Parent raw-file role; Windows remains a separate platform branch.                                                                                              | `khive-vamana/src/checkpoint_io.rs:176, :182, :215`                                                                                                                                       |
| Uncooperative SQLite writer fixture      | Added test-only default-VFS helper deliberately bypassing cooperative admission. Retain the seeding/observer population; it is neither a production caller nor an admitted same-worker peer.                                        | `khive-db/src/pool.rs:5`; `khive-db/src/pool/external_writer_fixture.rs:9, :15`; `khive-db/src/sql_bridge/handles.rs:95`                                                                  |
| In-memory settlement replacements        | Added or changed `open_in_memory` replacement/retired connections. Classified non-file matches; they cannot name a map or companion and add no file-opening class.                                                                  | `khive-db/src/raw_migration_settlement.rs:29`; `khive-db/src/pool/construction.rs:177`                                                                                                    |
| Timeout diagnostic output                | Existing append opener retained; ownership is now per log directory instead of one process-global sink claim. Parent output; retain each owner and later reopen.                                                                    | `khive-db/src/timeout_sink.rs:617, :636, :961`; `khive-db/src/timeout_sink/lifecycle.rs:1`                                                                                                |
| Note import database and inputs          | Added `db import-notes`: ordinary SQLite backend and independent caller-supplied NDJSON `File::open`. Apply parent database admission/refusal separately from parent raw-input disposition. JSON parsing follows the input open.    | `kkernel/src/cli.rs:1061`; `kkernel/src/import_notes.rs:157, :167`                                                                                                                        |
| Validation sort repair                   | Existing `kg validate --fix` entity/edge reads and rewrites moved to one helper. Parent raw read and output remain separate; rewrite now occurs only when record positions move. This is not covered by the export-output row.      | `kkernel/src/kg/validate.rs:202`; `kkernel/src/kg/validate/fixes.rs:7, :42, :86, :92`                                                                                                     |

For the moved validation group, the historical entity read/write sites were
`kkernel/src/kg/validate.rs:926, :952`, and the edge sites were `:960, :994`.
The current `validate/fixes.rs:42` read and `:92` conditional write both remain
in scope. Its actual acceptance needs unsorted valid input that reaches the
write, as well as the unchanged/no-write case. A malformed map alias may reach
the read and fail parsing before the write; that result proves no write-path
execution. Likewise, `import-notes` parsing failure after `File::open` is not
pre-open refusal. Both command paths need their actual direct-name/alias and
ordinary-peer arms. Parent lock survival is not map-content preservation or a
universal filesystem write barrier; W2's exclusions remain.

**Dependency and platform deltas.** `crates/Cargo.lock` changes `rusqlite`
0.40.1 → 0.41.1 and `libsqlite3-sys` 0.38.1 → 0.39.1. The newer bundled source
reports SQLite 3.54.1 (`libsqlite3-sys-0.39.1/sqlite3/sqlite3.c:470`). Its default
Unix main-file path still tries `findReusableFd` (`:47011`, called at `:47273`)
before `robust_open` (`:47338`); `unixUnlock` is at `:42938`. The newer rusqlite
bridge calls `sqlite3_open_v2` in
`rusqlite-0.41.1/src/inner_connection.rs:62`. These source references do not
rewrite the historical SQLite citations or the recorded probe's dependency
version, and are not evidence of a linked build or an executed gate.

The native VFS now answers `SQLITE_FCNTL_WIN32_GET_HANDLE` for the attested main
handle (`khive-db/src/code_map_vfs/callbacks.rs:447, :458`). The historical
missing-control prerequisite therefore no longer describes that source branch;
this does not establish a passing Windows pool or worker acceptance run.
`same-file` remains 1.0.6; Windows handle identity and Unix metadata identity
remain different paths. The shared no-follow and directory helpers do not make
Unix source evidence a Windows execution result. All required macOS, Linux and
Windows arms, including independent lock observations, remain.

The comparison uses a symmetric source-text search plus named-helper and
caller review. Search matches include tests, comments and non-file operations;
they are not executable-case counts. Imported aliases, traits and wrappers,
macros/generated code, transitive and platform-specific dependencies,
dynamically assembled SQL and subprocesses, and individually untraced fixtures
remain the residual categories stated above. No worker exists at this source
revision. Every retained or added actual path still requires the stated
worker/refusal/parent acceptance, including represented initial and later
connections, aliases, and closes. This delta is source evidence, not executed
worker acceptance, ratification, or a release of the implementation hold. A
source change after this pin requires another delta review.

### Architecture options as recorded

0. **Withdraw the in-process native VFS.** Keep Amendment 11's fence as the contract. Not proposed: the VFS has shipped (#4129).
1. **A narrowed in-process boundary plus worker-process isolation for residual raw readers.** Route or refuse SQLite connections and the named raw helpers in-process; move the rest to a worker. Not proposed: a remaining opener beside a guarded handle defeats the in-process part.
2. **An in-process boundary with a mechanical opener gate.** Disallow constructor and raw opener APIs outside reviewed admission wrappers. Not proposed: the gate must close every residual category above to support a whole-process claim, and a gate covering SQLite alone leaves raw closes outside it.
3. **Worker-process isolation, one worker per parent process.** **Recorded, not selected.** It retains worker lifecycle, operation-protocol, snapshot and source-byte transfer costs, but its shared quarantine and registration budgets and a restart or crash can interrupt other maps' operations within that parent. That failure domain does not satisfy #5614. Neither this option nor option 3a removes Amendment 9's protected-file objective or the #3593 production-handle gap by assertion.
   3a. **One worker per governed map within one parent.** **Selected.** Each stable canonical target key owns one worker reservation, within the parent limit of 16 and the 60-second idle-retirement rule above. A restart interrupts that map's operations only. Costs: a process and supervised lifecycle per live map worker, the same operation protocol and source-byte transfer, and admission/retirement of the bounded set. Acceptance records resident memory per worker and first-operation latency per map; no performance threshold is imposed.
4. **Format-level separation.** The worker stores a map so stock SQLite refuses it: a non-SQLite value in the header magic, translated by the guarded VFS, and companion files under names stock SQLite never derives. **Rejected** for W2. Stock SQLite acts on the derived journal and WAL names, plays back a hot journal and maps the WAL index through the main file's handle before it compares the magic (`sqlite3.c:64886–64895`, `:64961`, `:62944–62965`, `:68226`, against the comparison at `:76450`); `SQLITE_DBCONFIG_RESET_DATABASE` skips that comparison (`:76439–76441`); and a file opened in another database's journal role is written with no main-file check at all (`:65456–65494`). A refused open is therefore not evidence that nothing was written, and the scheme leaves exclusions (a) to (d) of W2 open while adding a file format, a conversion of every existing map and a loss of direct access for SQLite tools.
5. **Operating-system file authority.** The worker's map files are writable only under an identity the parent does not hold, a separate user or a sandbox that denies the parent write access to them. This is the only option recorded here that could support the universal property W2 does not hold. **Recorded, not designed.** Its costs: a second account or a per-platform sandbox policy as a deployment requirement; maps live inside arbitrary repositories (`<repository>/.khive/code-map.db`), in directories the parent's user owns, so the protected set is dynamic and its directories need their own authority; a change of ownership for every existing map; descriptors the parent inherited or opened before the change; and operator access to maps for inspection and recovery.

### Executable acceptance

Before sign-off, the first text's **routing-free lock-mechanism probe** runs on a plain temporary SQLite rollback database with the pinned bundled SQLite and no routing, parent snapshots, protected-path lists, identity ledgers or WAL transitions. It reproduces the first text's three losses in one process, all close-unlock on Unix: a second descriptor's close, a default-VFS last-shared whole-file unlock, and a default reader's lock-type downgrade. Two further arms record what the Unix losses do not cover. On Windows a second handle cannot take, unlock or release by closing the owner's byte-range lock; that arm shows survival, not a loss. After a rename, truncating through the stale path empties the replacement while the renamed original keeps its lock and its content. Each arm uses one lock per fixture, an independent-process observation before and after (`F_GETLK` on Unix; on Windows a nonblocking exclusive `LockFileEx`, which reports a conflict but not its owner), and the same fixture without the second client as a survival control. The Unix cases run on macOS and Linux with fixtures on each environment's own filesystem, the Windows arm on Windows, and the stale-path arm on all three. The first stage of acceptance runs the same operations from the parent against a worker-held lock; each must leave that lock intact, and this pair is the evidence for the boundary.

For every census row, invoke its **actual opening path** with direct names and same-inode aliases, every represented initial and later connection class and a pre-existing ordinary peer. Each arm shows the worker disposition (the open's process id recorded as the worker's), the typed W2 refusal before SQLite opens the file for a parent constructor that names a map, or, for a parent raw reader, that it ran in the parent while the worker's lock survived. Each arm runs the real caller; a mock or a substituted open does not count, and a path claimed unreachable is shown by the same executed probe.

Arms specific to this architecture:

- **Parent ownership.** A daemonless `kkernel` invocation reaching two map keys starts a distinct worker child at each key's first operation: the children hold the descriptors for those governed operations and the invoking pid holds none for them, without widening W1 to W2's explicit parent-opener exceptions. Repeated and concurrent operations for one key, including replacement of its main file or pinned parent at the same canonical pathname, retain one reservation and one worker; registered-name reads and ingest of that target converge on it. Repeat while a daemon runs and show that the invocation still owns its children rather than using the daemon's workers. Ending the parent ends every child worker, including when it is killed during a map read (`SIGKILL` on Unix, `TerminateProcess` on Windows): within five seconds each worker has exited and holds no descriptor on its map, and a quiescent transition on each map then succeeds. Removing one worker's lifetime pipe makes that worker survive and its transition refuse while it lives. Before the kill, start an unrelated child that outlives the parent; every map worker still exits within five seconds. Making a lifetime pipe's write end inheritable makes the surviving child keep that worker alive. Two distinct parents operating on one map retain Amendment 12's native locking, bounded busy timeout and transition-conflict controls. Include the raw snapshot-copy path before its constructor and show that its first map read is in that map's child.

- **Registration identity.** During parent runtime construction, register an existing map and record that its identity/header read occurs in its key's worker; the parent receives only owned identity metadata and retains no map descriptor for that registration. A later registered read compares against that recorded identity in the worker. Replace the main file, idle-retire or recycle the worker, and show that a successor still refuses the stale parent identity. Restoring the original identity admits the read. Moving the initial read back to the parent, omitting the later comparison or refreshing the parent's identity on worker restart must each fail its named ownership or stale-registration assertion.
- **W3 falsifier.** On each Unix host, while A's worker holds A's map lock, open and close a raw hard link to A from the parent, then run each W3 worker path on A with its guarded admission replaced by the raw helper it replaces; an independent process observes A's lock after each. The parent arm keeps the lock, each raw arm inside A's own worker loses it, and guarded admission keeps it. Separately hold B's lock in B's worker and open and close an alias of B from A's worker: B's lock survives because its owner is another process. That cross-worker observation is not a same-worker loss control or permission for arbitrary content writes. Keep W3's guarded identity, deferral and protected-file checks for each worker's own admitted generations.
- **W2 race.** A parent writable pool opens a map that carries the application ID through a hard link the path check does not name: it refuses before SQLite opens the file, and the map's bytes read back unchanged. With the application-ID check removed, the same arm shows the header changed to WAL. A second arm, exclusion (e), opens a map with no application ID through that hard link: the parent's open succeeds and writes it, and the worker's next ingest refuses the map on its link count; after the parent's pool closes and the link is removed, the ingest returns the file to rollback mode and writes the application ID, and both read back after it. A third arm creates a default-target map through the worker and points a parent writable pool at it through its own path and through a hard link: the path check refuses the first, the application-ID check the second, and neither changes the file.
- **W2 binding order.** Between the admitted-target constructor's pre-open sample and SQLite's open, a deterministic barrier swaps the target for a disposable map fixture that has a genuine hot journal under the target's derived journal name: the binding refuses before the connection's first SQL statement, and the fixture's bytes, size and modification time and its journal read back unchanged. With the binding after the `main.sqlite_master` read, as at the census revision, the same arm shows the journal played back into the fixture. A path absent before the open that receives a map at the same barrier is refused as not empty; with the emptiness check removed, the parent's identity bootstrap writes the map. A pooled reader, initial and replacement, opened across the same barrier is refused before its first SQL statement, shown by an SQLite trace callback installed at the open; with the reader's binding after its connection pragmas, as at the census revision, the trace records those pragmas on the swapped file first.
- **Attach.** On a parent writable connection and on a worker connection, with each authorizer khive installs in place, `ATTACH` of a map by its path, through a hard link, as a URI, as a bound parameter and as an expression each fails at the authorizer before SQLite opens the file, and the map's bytes, size and modification time read back unchanged; `ATTACH ''` succeeds. A configured ordinary backend with matching canonical path and file identity attaches and supports a schema-qualified query through the bridge writer; an unconfigured ordinary file, a different canonical path and a replaced identity at the configured path each refuse before the file opens. A configuration that declares a map as a backend, by the map's path, through a hard link, and, for a map carrying the application ID, at a path that is neither registered nor of the default-target shape, fails runtime construction with the named error before SQLite opens the map, and the map's bytes, size and modification time read back unchanged; with configured backends opened through the ordinary pool constructor, as at the census revision, the same arm opens the map. A configured ordinary backend replaced by another ordinary database between the `ATTACH` statement's prepare and its execution is detached and refused before the connection's next statement; with the post-attach binding removed, the next statement reads the replacement. A script through `execute_script` whose first statement attaches a configured ordinary backend with matching canonical path and file identity, and whose second statement reads from it, is refused when the `ATTACH` is prepared, and the second statement does not run; the same `ATTACH` on a raw handle of a khive connection is refused when it is prepared, and a statement list through `execute_batch` whose first member is that `ATTACH` and whose second reads from it is refused when the `ATTACH` member is prepared, and the second member does not run. The batch route prepares every member before executing any, and a member whose preparation fails there is prepared again at its turn (`khive-db/src/sql_bridge/rows.rs:105–118`, `batch_exec.rs:18–26`), so the refusal arrives at that second preparation. With the single-statement condition removed from the authorizer, the script and the statement list each attach the backend and run their second statement before any binding, and the raw handle attaches it with no binding. The replacement in these arms is an ordinary database: a replacement map's hot journal is played back inside `ATTACH`, before any binding can run, which is exclusion (h). Removing either the path or identity comparison must fail its corresponding refusal arm, and a blanket file-attachment refusal must fail the configured-backend arm. `VACUUM` succeeds on each ordinary khive database through the production maintenance route (`khive-storage/src/sql.rs:39–51`). With the attachment rule removed from the authorizer, the named statements attach the map. With the attached-database limit set to zero, or the empty-name attachment refused, the `VACUUM` arm fails. A caller's `ATTACH` through the bridge writer obeys the same configured-backend exception and map refusal. The four test fixtures that attach a scratch file at the census revision (`khive-db/src/pool_tests.rs:673`, `:945`; `sql_bridge/direct_busy_tests.rs:135`; `writer_busy_fixture.rs:358`) are rewritten to reach that file through a connection of its own, and the two bridge-reader refusal tests (`sql_bridge_tests.rs:331`, `:586`) keep passing.
- **Channel access.** On Unix the socket has mode `0600`, and an unexpected peer uid is refused at each endpoint before any operation or source bytes cross. On Windows the pipe's security descriptor admits only the parent's user, a second instance of the pipe's name is refused, and a peer process other than the expected one is refused at each endpoint before any operation or source bytes cross. A worker request for a path the operation did not name (W3), and a named path whose contained open fails because it resolves outside the operation's root, are each refused without reading the file. Removing either endpoint's peer check, on either platform, or the named-path bound must fail its corresponding arm.
- **Restart reclaim.** In separate fixtures that reach the real limits without an earlier limit masking them, trigger A's quarantine drain by retention reaching its cap, and its registration and ledger drains by a refused next charge as W6 specifies. First show that a successful 512th registration and a successful ledger reservation exactly filling L do not themselves drain the worker; a control that drains on either successful equality must fail its accepting-worker assertion. Hold a registered B read in flight through an actual barrier, observe A enter cap-triggered draining, and let A settle, exit and be reaped without an operator restart or a 60-second idle wait. Keep sending new requests to A during draining: none is admitted into the old worker or postpones its exit. The next clean request starts A's successor; record its three resource counts at their empty values before new admission. With cap-triggered draining removed, the old A worker persists and the named recycle/next-clean-operation assertion fails. A has a new worker pid; B keeps its pid, lock and successful in-flight read. Inventory every parent descriptor before and after: only A's control, lifetime and supervision endpoints may change, no A data descriptor for the fixture's governed operation is present in the parent, and every other existing descriptor, including W2 exception-route descriptors and B channels, is unchanged. Recreated IPC endpoints are an explicit exception to the earlier literal whole-table-equality wording, not permission to change parent database ownership. Worker restart does not refresh a stale parent registration identity. Registered-A recovery must restore its original registered main-file identity and pathname or use a separately supported explicit re-registration; success of an unregistered ingest after restart is not proof of registered-read recovery.
- **Windows role-collision recycle gap.** Use an unregistered explicit A ingest target so no parent registration refusal masks this arm. Through the real guarded companion route retain a closed companion's physical identity and role in A's worker by leaving that file present without a successful VFS delete; record that actual retained role, not an injected ledger row. With all A connections closed, prepare a valid disposable SQLite main in that same physical object without changing its Windows file identity and place it at A's unchanged main pathname, removing other stale companions. The actual ingest must first refuse the recorded cross-role identity and cause A's automatic drain and reap. Without removing or replacing that object, restarting the parent or invoking an operator restart, retry after reap: A's successor has no old role record and admits the otherwise valid main. Record the unchanged object identity across refusal, reap and successor admission. This arm records the explicitly retained gap; it is not a refusal-after-restart safety claim. Keep another map B's worker and in-flight read alive throughout to preserve the failure-domain assertion.
- **Ledger bound.** In a dedicated Unix child fixture, set a real soft `RLIMIT_NOFILE` of 10 so the production calculation gives effective L=5. Startup must return the typed minimum-ledger-cap refusal naming observed 5 and required 6 before any map admission. A control restoring only the old below-3 startup guard must fail that named minimum-cap assertion; spawn, loader, descriptor-allocation or fixture-setup failures are not this proof. The separate L=6 settlement fixtures below retain sufficient actual descriptor headroom while selecting their reduced ledger cap, and prove that one initial plus one replacement generation fits without latching. Ten thousand write transactions on one map leave the ledger's entry count where it was after the first; with journal retirement removed, the same run grows it by one per transaction. Run the bounded-ledger settlement arms in three separate fixture runs, one for a write, one for a pool reopen and one for a quiescent prior-WAL transition; never pause those three operations concurrently on one main. In each run use L=6, retain one closed older linked main generation at charge three, and admit the current main generation with its companion reservations at total charge six. Equality with L must leave that worker accepting operations. The transition run seeds a valid prior-WAL current main and closes every other code-map connection before admitting the transition; the write and pool-reopen runs use their own rollback fixtures. Admit only that run's operation and pause it before its next relevant guarded main or companion open. At that barrier, temporarily replace the current main with one further physical generation at the exact same stable pathname and drive that existing worker's actual guarded reservation attempt; a different pathname and a new worker cannot execute this arm. It must refuse the three-slot charge before an OS open, leave charge six unchanged and latch draining inside this fixture run. Restore the admitted current main's exact identity at that pathname before releasing the paused operation; no new generation is admitted and no synthetic cap count substitutes for the refused reservation. The already admitted write completes its transaction, the separate reopen arm opens the same admitted main in a new pool, and the separate transition arm finishes its quiescent transition; a main-file open of a new physical generation under that same stable map key is refused before the file is opened, as a request error naming the cap, and creates no file. Then observe automatic draining and reap. This reduced ledger test configuration proves reservation semantics only; it cannot replace the effective production L accounting or the real 512-registration acceptance ramp. Also exercise the literal full-cap case at L=4,096: use same-parent ordinary DELETE-mode physical-generation replacements that reuse the rollback VFS registration, preserving older linked main pins, until charge 4,095 is recorded. The next three-slot main reservation must refuse without opening the file or changing charge 4,095, trigger automatic draining and reap, and permit the next clean operation in a successor. Keep the registration count below 512 so it cannot mask this ledger-reservation refusal. A control that drains only when occupancy equals L must fail the named refused-reservation/recycle assertion; 4,095 never equals 4,096. With the reopen rule removed, the WAL transition on the admitted map is refused at its first main-file open (`transition.rs:201`). A reopen whose name is swapped to a new file between its resolution and the open is refused at the cap and creates no file. With the companion reservation removed, a write transaction on a map admitted at the cap is refused at its journal open.
- **Transition restart.** Stop the worker after the transition's `journal_mode=DELETE` and before its companion cleanup, once with the persisted `-wal` alone and once with a `-shm` left from the map's WAL past beside it. In each, the next ingest through the restarted worker completes, and the map reads back in rollback mode with neither companion and with the content its source produces. With the recovery removed, both arms are refused as a rollback header with a companion. In the `-wal`-alone arm seed committed WAL frames so its measured length immediately before the mode-switch close is strictly positive. Use a test connection hook immediately after the transition opens its own connection and before the production `PRAGMA journal_size_limit=-1` runs to set that same connection's limit to 0, and record the seeded value. Apply this hook in both the pristine and control runs; changing only a different connection or relying on the compiled default does not execute this arm. Record `journal_size_limit` again from the transition's own connection after its production pragma and before its FULL checkpoint. At the post-switch/pre-cleanup stop, first assert that the persisted WAL retains exactly that positive measured length, then assert that the recorded limit was -1. With that `PRAGMA` setting 0 instead, the same arm leaves a zero-byte WAL because the close truncates a persisted WAL under a non-negative limit (`sqlite3.c:69983–69990`); the named retained-positive-length assertion must fail. Add a separate control that removes only the production `PRAGMA journal_size_limit=-1` statement while retaining the same connection hook, seeded 0, WAL contents and stop barrier. The connection remains at 0, the persisted WAL is shortened to zero at the mode-switch close, and the same retained-positive-length assertion must fail. Restore the production statement and rerun the same arm: the observed limit changes from the seeded 0 to -1 and the original positive length is retained. A failed readback or setup error is not this control's proof. Subsequent ingest may still succeed in either mutant, since a WAL with no commit frame recovers unchanged, and that common recovery outcome is not either control's oracle. A third arm commits one transaction to the stopped map through stock SQLite before the restart (the left `-wal` makes it open the map as a WAL database, exclusion (b)), in a process killed before it closes, so the new frames stay in the `-wal`: the ingest is refused with an error naming both files, and the main file and the `-wal` read back unchanged; with the page comparison removed, the `-wal` is unlinked and that transaction is lost. Editing a frame by hand is not this arm, because a frame that fails its checksum is not recovered. A fourth arm writes a `-wal` with valid frame checksums whose frames equal the main file's pages but whose last commit records a smaller database size: the ingest is refused naming both files and both read back unchanged; with the size comparison removed, the `-wal` is unlinked. With a WAL-mode connection from another process holding the map, the arm is refused at the exclusive lock and both companions are still there. With another process holding the `-shm`'s dead-man-switch byte, the `-shm` arm is refused and the `-shm` is still there; with that lock check removed, the `-shm` is unlinked.
- **Companion content after retirement.** In every arm A is an explicit ingest target never registered with the parent, and the operation is the actual `code.ingest` route, not a substituted VFS/helper call. First successfully ingest A, close its operations, and let its worker actually retire after 60 idle seconds and be reaped. Preserve the closed A main as a distinct physical generation; rename it onto a companion name of A's same canonical key and install the arm's replacement main at A's unchanged pathname. The successor has no role record for that old generation. No parent restart, re-registration or registration-identity refresh is used; a parent stale-identity refusal cannot stand in for the content predicate. For every arm record the actual role-specific open or destructive-removal content predicate reached through `code.ingest` before crediting its refusal. A genuine SQLite database header on the exact companion object must refuse before use or removal, and the old database and replacement main read back byte-identical. Run (1) a journal-role open beside a populated replacement main; (2) a journal delete beside a zero-length replacement main, which reaches deletion without a journal open; (3) a transition-WAL-role open beside a valid prior-WAL replacement main; (4) recovery unlink of a WAL with no valid commit frame beside a rollback replacement main; and (5) recovery removal of SHM beside a rollback main with otherwise recoverable companions. In (5) refusal precedes the destructive lock/removal sequence. An open-side control removes only its role's open content check and must fail the named refusal-before-first-use assertion at that guarded admission; a later removal refusal does not satisfy that observation. A direct removal arm that has no earlier role open removes only its removal content check and must fail its own unchanged-bytes/refusal assertion. Where recovery first opens or reads the companion, add a separate removal-recheck arm: admit an otherwise valid companion, then at a barrier after the earlier content/recovery reads but before the destructive recheck write the database header into the same admitted object without replacing its identity. Record the two files' bytes at that barrier. The removal recheck refuses and preserves those bytes; removing only that recheck deletes the disguised database and fails its named assertion. This control does not disable no-follow or identity checks and does not claim to prevent a hostile write after the final content check. A still-active earlier content gate is not a discriminating removal control. The zero-main journal control exposes SQLite deleting the old database without opening it, and the WAL/SHM removal controls expose cleanup deleting a database-shaped companion. These are same-key successor fixtures, not assumptions about a shared cross-map ledger. Repeat the content refusal after an explicit worker failure/restart. No control bypasses the no-follow, protected-identity, pinned-parent or safe-close checks merely to reach the content predicate.
- **Replacement churn.** At A's same canonical pathname, repeatedly install a valid, quiescent prior-WAL fixture and invoke the actual `code.ingest` transition until A's worker fills all 512 registration slots, then request one more fresh transition registration to execute the refused-charge trigger. The successful 512th registration must not itself drain the worker; the next new registration attempt must refuse without increasing the count above 512. Each fresh transition needs a new guarded transition registration before rollback-writer cache lookup; an ordinary DELETE-mode replacement that reuses a rollback registration is not this arm. Record the starting total and every real increment rather than assuming a fixed number of replacements. Every replacement retains the same key and worker pid, not a new reservation per native identity. The typed refusal reports A's cap-triggered recycling; A automatically drains, exits and is reaped, and a valid next operation opens the map in its successor without an operator action. If A is registered, the parent's stale main-file identity refusal is distinct and cannot be cleared merely by restarting the worker; use an actual supported route whose parent registration remains valid, or an unregistered ingest target, to reach and measure the worker budget. Do not refresh parent registration silently to manufacture reclaim.
- **Failure-domain availability (#5614).** First complete a clean admission of A so the failed-first-admission rule cannot recycle it during the ramp. Keep A's key and worker pid stable until the cap latches; queued or in-flight work prevents idle retirement before that point, and the fixture records A's subsequent automatic drain/reap separately. Throughout the ramp, actual registered-map reads of B and actual `code.ingest` operations on a third target C succeed. Put C on a separately identified volume from A and from any concurrent parent writer, and record both volume identities; a lease timeout on C fails its availability assertion, never counts as successful isolation. B's read uses its already established registered worker and its own measured lock. Run three distinct budget cases against disposable fixtures through the real public operations and their guarded opens:
  - Reach the current per-domain quarantine cap **8**, where the domain is the pinned parent's native identity and exact leaf. This proves its own refusal but is not alone a shared-worker failure control.
  - Reach the actual worker-wide quarantine cap **32** by replacing the parent directory at A's **same canonical pathname**, producing distinct native parent/leaf domains while retaining A's logical key and worker pid. Through actual `code.ingest`, use a barrier after target/protected preflight and before the native header open in `prepare_rollback_target` to introduce the protected alias. That header open precedes cached rollback-registration lookup and its pinned-parent mismatch check. Restore a clean target before each attempt so preflight does not refuse before any descriptor is retained. Use five fresh parent-identity domains with retained-descriptor counts **7 + 7 + 7 + 7 + 4 = 32**; none reaches the per-domain cap 8 before the global cap 32. Record each retained protected-alias descriptor and observed count; fresh parent identities allow separate domains at the same pathname without resetting the worker's total. The separate cap 8 fixture must recycle when its own domain cap latches and cannot be a prefix of this global ramp. A registered-main identity refusal or cached-parent mismatch before the guarded header open does not execute this arm. Do not replace the domain or count with a synthetic value, change A's pathname key, or treat worker restart as parent registration refresh.
  - Separately fill the actual registration cap **512** through the valid quiescent prior-WAL replacement/transition route above under A's same key and worker pid, then trigger draining with the next refused new registration. Successful equality at 512 must preserve the worker until that refused attempt. Use a bounded fixture with at most one initial A main generation and at most one additional main generation for each fresh transition registration. Through the attempt that exceeds 512 registrations this charges at most 513 main-generation reservations, or **1,539 of the 4,096 ledger slots in this fixture**, even when every older linked main stays pinned. The fixture must start with effective L=4,096; a smaller ordinary-production L is not this ramp. Genuine journal/WAL deletion retires its companion entry as W6 requires; the fixture must not rename extra companions away to accumulate unreserved entries. At this source, each distinct registration retains an `Arc` to its guard and that guard's one pinned target-parent handle (`vfs.rs:43–48, :119–147`; `unix.rs:22–25`); configured protected parents are inspected with transient pins by `sample_protected`, not retained once per registration (`mod.rs:1003–1037`). Fix and record the protected-set size and directory depth, serialize A's transitions, and close each completed operation's connections; count all surviving parent pins, ledger anchors, quarantine descriptors, live connections, IPC and deferred-close descriptors rather than counting registrations alone. On Unix the fixture establishes a real soft `RLIMIT_NOFILE` of at least 8,192 within the available hard limit before spawning its workers, verifies that W6's startup calculation gives L=4,096, and records the actual descriptor high-water mark strictly below that real soft limit. On Windows record the corresponding actual handle high-water and successful headroom for the same bounded topology. The number 8,192 is this fixture's prerequisite, not a claim that it supports every configured deny set or a production startup requirement. Insufficient host headroom is a failed prerequisite that must be corrected before this arm passes; an earlier resource failure is not an expected successful result. Neither lower either cap, retire linked old pins nor fake a count. At exhaustion, C's actual ingest must need a **fresh C transition registration**, using a fresh quiescent prior-WAL fixture at C's target while C's own worker budget stays below its cap, and must show the actual registration increment in C's worker. Reuse of an existing matching rollback registration, including a cached DELETE-mode C route, is not this control, because matching registrations are reused before the cap check.

  Record the complete counter, key/pid, typed-refusal and B/C operation-result timeline. While a B read is held in flight, let A automatically drain, exit and be reaped after its cap, show A's empty successor resource state under a new pid, and let B finish successfully with the same pid and lock; A's next clean operation succeeds without an operator restart. The shared-worker control repeats the actual public work with A, B and C assigned to **one shared worker**. For the quarantine case, separately require the named **B registered-read availability** assertion to fail at B's actual guarded open and the named **C ingest availability** assertion to fail at C's actual guarded open, both because global quarantine admission is exhausted; C alone is insufficient. Admit and pause those operations just before their guarded opens before A reaches its cap so cap-triggered draining does not replace the intended guarded-open failures with a generic new-request refusal. For the registration case, pre-admit and pause C just before its fresh-transition registration; the named **C fresh-registration availability** assertion must fail at the exhausted registration budget. A cached B read may still succeed for this budget and is not required to fail. In a separate shared-worker lifecycle control, while B's read is held in flight, force the shared worker to stop and restart: B's in-flight read must be interrupted, so the named **B read survives A recycle** assertion fails; merely exhausting a shared budget without testing the shared restart is incomplete. The corresponding isolated-worker fixture stops or recycles only A and B completes. Require each named failure at its own semantic observation, not a build, setup, unrelated timeout or generic error. Replacing stable-path keying with file-identity keying must fail the one-key/one-worker assertion during the same churn. Counter resets, substituted opens, only a per-domain exhaustion, reused C registrations or an A worker retired before the cap do not prove the ramp.
- **Parent worker bound and retirement.** Start 16 distinct map workers and keep them non-idle through real operations. A seventeenth distinct key receives `worker_capacity_exceeded` naming 16 before any child spawn or map operation; an admitted key still works without another slot. Observe starting, stopping and unreaped children in the count. With no other key waiting for a slot, let one worker become idle: it stays until 60 continuous monotonic seconds of the defined idle state, closes and exits, and releases its slot only after reap; the next request for that key lazily starts a new worker. A request racing retirement either wins admission before retirement starts or waits for that key's prior child to be reaped before a successor starts. Keep a transaction, queued request, source-byte exchange, pending reply and cancellation/settlement cleanup active in separate arms and show none is evicted as idle. Removing the parent bound, counting only running children, freeing before reap, or dropping one of those idle guards must fail its named capacity or lifetime assertion. No busy-worker eviction or in-process fallback is allowed. Slot yielding, in separate arms: parent construction with more than 16 registrations succeeds and a read through each registration returns its data; with 16 admitted workers all idle, a seventeenth distinct key is admitted after exactly one idle worker exits and is reaped, and that worker's key is served again by a successor afterwards; with 16 non-idle workers the seventeenth key is refused as above. A control that keeps idle workers on their slots must fail the construction and seventeenth-key-admission assertions. In separate first-admission fixtures, send more than 16 sequential hard-link aliases of one map, each refused on link count, and nonexistent explicit leaf targets, each refused on existence. After each refusal settles, observe prompt exit and reap without the 60-second idle interval; a different clean map admits immediately after reap. Repeat with concurrent requests: failed-key slots never exceed the current in-flight admission/settlement/reap population or the global bound 16, and settled failed requests leave no idle reservations. Retaining failed workers for the idle interval must fail the named immediate-clean-map/capacity assertion. A successful prior admission of A remains required for the quarantine exhaustion fixtures.
- **Failure.** Make A's worker startup fail on its first map operation: the caller receives a typed error naming that failure, and the parent opens no map descriptor. Kill A mid-read and between a write's commit and its reply while B has an in-flight read. A's read returns the typed retryable error; its write returns outcome-unknown and a read through A's restarted worker resolves it. B keeps its worker and completes its read; the parent performs no map open of its own. Preserve W4's per-transaction settlement and repeated-ingest recovery for interrupted multi-transaction ingest.
- **Lease.** A worker call made while the calling parent thread holds a same-volume lease is refused or reported at the call, never a wait to the lease deadline. Separately run up to 16 workers and sustained parent writes on one recorded volume, using actual lease acquisitions and recording each worker's wait, success or typed timeout and the operation deadline. A starvation timeout is permitted evidence of the explicitly unprovided cross-process fairness, not an availability pass; an unbounded wait fails. This same-volume observation is distinct from #5614's C-on-another-volume availability arm and cannot substitute for its successful C operations.
- **New opener.** A newly added map-aware open in the parent fails the dispatch-or-refuse boundary, as do a newly added unclassified SQLite **and** raw opener, a parent call to a code-map constructor and a parent registration of the guarded VFS; this tests census closure rather than only the listed paths. A gate run that selects no case, or a scan that finds no opener, fails.
- **Exclusions witness.** On disposable fixtures, never on a live map, exclusions (a), (b) and (c) of W2 are each run with stock SQLite and change the map: a hard link to the map at an ordinary database's `-journal` name before that database's first write in a transaction, a stock open of the map with a hot journal under its derived name, and `SQLITE_DBCONFIG_RESET_DATABASE` followed by `VACUUM`, each recorded as the map's bytes and companion names before and after. On Windows, second names through a symbolic link and through a directory junction record the physical identity of every derived companion, for exclusion (f). For exclusion (h), a configured ordinary backend is replaced, between an `ATTACH` statement's prepare and its execution on the bridge writer's single-statement route, by a disposable map fixture carrying a genuine hot journal under its derived name; the arm records the fixture's bytes and journal before and after, and passes when the journal is observed played back inside `ATTACH` and the post-attach binding then detaches the schema and refuses the statement. These arms pass when the change is observed. They keep the excluded routes visible: a later revision that claims one of them closed turns its arm into a refusal arm with its own control.

Every Unix host gate includes **both directions** for close, whole-file unlock and lock-type replacement, observed independently before a default or raw client opens, during its operation, after a completed read transaction and after its close, with transaction unlock observed separately from the final descriptor close. A same-process `F_GETLK` does not report the process's own lock as a conflict, and a local lock table is not kernel-lock evidence. Removing the worker boundary, a refusal or guarded admission must make the corresponding named arm fail.

Unix and Windows gates keep Amendment 12's native identity and alias refusal, protected-file presence, bytes, size and modification time, prior-WAL transition, hot-journal recovery, DELETE rebase concurrency and zero-`xShm*` controls. Unix also keeps the retained-handle pin controls of the 2026-10-05 ledger correction. Windows also runs the Channel access arm against the named pipe. Non-adversarial quarantine occupancy stays zero. Record the exact source revision, commands and selected tests, OS and filesystem, the actual open, unlock and close events and independent before-and-after lock observations.

**Resource and latency record.** Record resident memory for each map worker and first-operation latency for each map, with the source revision, OS/filesystem, worker count and measurement method. These observations describe the per-map process cost; there is no memory or latency threshold. They do not replace any safety, budget or actual-route assertion.

### Probe record (2026-10-19)

Sequencing step 1. The routing-free probe of the Executable acceptance section ran at source commit `abf9b9e236b191130736e430c19a2997fb21713e` (#5756) on main `a48cf947db04dfe108ab4aa611e93713feb6112d`, with the bundled SQLite 3.53.2, the stock `unix` VFS on the Unix hosts and rollback journal mode. The probe lives in `crates/khive-db/tests/lock_mechanism_probe.rs` and its `unix`, `windows` and `stale_path` modules. Each case runs once with the second client and once without it as the survival control. Every observation comes from a child process, the test binary re-executed, whose process id differs from the holder's: `F_GETLK` for a write lock on Unix, which reports the conflicting type, range and process id, and a nonblocking exclusive `LockFileEx` on Windows, which reports a conflict but not its owner.

Commands: on Linux and macOS, `cargo test --locked -p khive-db -- <names>` selecting the three Unix cases and the stale-path case by name, with each test's assertions as the observation check; on Windows, the CI steps run `cargo test -p khive-db --test lock_mechanism_probe <case> -- --exact --nocapture`, which prints every observation.

Hosts: Linux x86_64, Ubuntu 24.04, kernel 6.8, fixtures on ext4; macOS 27.0.1 arm64, fixtures on APFS; Windows on the hosted runner, fixtures on NTFS (GitHub Actions run 39192177229).

- `unix::unix_second_raw_descriptor_close_releases_process_lock`, Linux and macOS: lost. Byte 0's write lock reads unlocked after the second descriptor closes; the control keeps it.
- `unix::unix_default_vfs_last_shared_unlock_releases_unrelated_byte`, Linux and macOS: lost. Byte 0 is held during the default reader's transaction and reads unlocked after its COMMIT, before its close; the control keeps it.
- `unix::unix_default_reader_downgrades_process_write_lock`, Linux and macOS: replaced. The write lock at `SHARED_FIRST` reads as a read lock over the reader's 510-byte shared range, then unlocked after COMMIT; the control keeps the write lock.
- `windows::windows_handle_interaction_does_not_release_another_handles_lock`, Windows: survives. The second handle's shared lock request and its unlock are both refused (`ERROR_LOCK_VIOLATION`, `ERROR_NOT_LOCKED`); the owner's lock reads as a conflict before, during and after the second handle's close, and unlocked only after the owner's own unlock; the control matches.
- `stale_path::stale_path_truncate_after_rename_changes_the_replacement_while_original_lock_survives`, Linux and macOS: the replacement is emptied, and the renamed original keeps its lock and its row; the control keeps both. Windows: the replacement is emptied (8192 bytes to 0); the renamed original reads as a conflict before and after the stale client, unlocked only after its owner releases it, and keeps its row; the control keeps the replacement's 8192 bytes.

Each Unix and stale-path case also ran with a deliberate defect, and each failed at the phase its defect predicts (Linux and macOS alike): the second descriptor leaked instead of closed (the lock still reads write where the loss arm expects unlocked); the observer asking for a read lock (the downgraded range no longer conflicts, so it reads unlocked where read is expected); the reader's transaction touching no table (no shared lock is taken, so byte 0 stays write-locked after COMMIT); and the stale-path client opening without truncation (the replacement keeps its bytes). These four controls show the observations depend on the second client's operation and on the observer's request, and are not fixed outcomes of the fixture.

The three Unix losses are reproduced on both Unix hosts. On Windows a second handle cannot take, unlock or release the owner's byte-range lock, which is the survival this section expects there.

### Sequencing

1. **Probe.** The routing-free probe under Acceptance runs, and its results are recorded beside this text.
2. **Sign-off.** With the recorded probe results, the maintainer signs off this revision as a whole: the selected option 3a, its stable-key and bounded-worker contract, the census and current-base delta, and the Windows withdrawal of the lifetime clarification. Sign-off approves implementation on a branch; nothing in this text takes effect before ratification.
3. **Implementation** of the worker on a branch, under the contract and against the census population, with a delta census at its base. The configured-backend refusal (W2, _Configured map backends_) merges no earlier than the registered-map read verbs (ADR-139) that replace B7's dedicated-configuration read path, so no revision has neither read path.
4. **Executed acceptance** against that branch, as above.
5. **Ratification, then merge.** The amendment becomes Accepted when the executed acceptance passes for the full census population, and the worker merges after that, which is the first text's release condition. A signed-off text is not executed evidence and does not ratify.

## Amendment 14 (2026-10-02): sequential recovery of interrupted L2 sweeps

**Status: Accepted; ratified by the maintainer, 2026-10-03.**

**Amends:** Amendment 2 B5's L2 freshness protocol. **Retains:** deterministic
project/module/symbol ownership, observed `last_seen_at` and per-project/language
`sweep_clock`, historical and manually authored edges, the existing secret gate,
and the existing L1/L1.5 clock behavior. This amendment does not change Amendment
12, Amendment 13 or their implementation and ratification holds. It specifies
new project properties and recovery behavior; it does not assert an implemented
or verified recovery path.

### Interrupted runs and the sequential guarantee

The current project clock advances before file and edge work. A failed or
cancelled L2 invocation can therefore retain a committed prefix: some natural
edges carry an earlier sweep stamp, while others already carry the failed
invocation's stamp. A subsequent unchanged-file fast path that refreshes only an
exact predecessor stamp cannot recover both populations. Recording only a
completed timestamp is also insufficient once a failed invocation has committed
some edge refreshes. Widening refresh to older timestamps would promote retained
removed references and is not the recovery rule.

Recovery is guaranteed for **sequential invocations** of one
`(source_project, language)` owner. Attempt and completion identities are not
leases, fences or a serialization mechanism. This amendment establishes no
correctness guarantee for overlapping writers of that owner, including writers
using separate runtimes or processes. Existing per-row compare-and-set/rebase
behavior remains; no clock maximum, stale-lease stealing, process-local mutex or
new serialization is introduced. A caller-supplied sweep time may repeat or move
backward and must not serve as invocation identity or invocation order.

### Versioned durable owner state

The project entity retains `properties.sweep_clock` and gains
`properties.l2_sweep_runs`, an object keyed by the same language used for that
owner's sweep clock. Each participating owner's language entry has this shape:

```json
{
  "version": 1,
  "attempted": {
    "run_id": "b024eb06-bd98-44c5-8542-74b49df6e528",
    "sweep_time": "2026-10-02T04:00:00+00:00"
  },
  "completed": {
    "run_id": "b024eb06-bd98-44c5-8542-74b49df6e528",
    "sweep_time": "2026-10-02T04:00:00+00:00"
  }
}
```

The example is a completed invocation, not a shared identity. `run_id` is a
fresh, unpredictable UUID v4, stored in canonical lowercase dashed spelling.
One invocation retains the same identity for every write of a given owner;
repeated upserts of that owner do not create new attempts. `sweep_time` is the
exact string emitted by that invocation's existing sweep-time serialization. It
is compared as opaque text, not normalized or ordered. The new reader does not
add RFC3339 validation to legacy sweep clocks or edge metadata.

`version` must be the JSON integer `1`. `attempted` must be an object containing
exactly the string fields `run_id` and `sweep_time`. `completed` is either `null`
or an object with those same two fields. The owner entry has exactly the three
fields shown. Unknown versions, unknown owner-entry fields, missing fields,
wrong types or noncanonical/non-v4 run IDs provide **no reuse authority**. A
missing or malformed `l2_sweep_runs` object or target-language entry likewise
requires recovery. An incomplete attempt is valid stored state, but is not a
completed predecessor. These checks govern the new markers only; they do not
introduce stricter legacy project-kind, language, namespace or timestamp
predicates.

A predecessor authorizes the unchanged-file fast path only when its new entry
is valid, both markers are non-null, their run IDs and sweep-time strings are
identical, and the retained `sweep_clock[language]` is exactly that completed
sweep-time string. Capture that authority once, before any selected-L2 path
advances the owner's visible clock, including an earlier selected L1 or L1.5
upsert. A later L1-only invocation may advance the visible clock without changing
L2 markers; the resulting mismatch requires real L2 recovery on the next L2
invocation. No-L2 invocations create neither new L2 attempts nor completions.

The current attempt is written in the existing `upsert_project` mutation that
advances the visible clock; it adds no separate attempt mutation. Preserve a
valid predecessor's completed marker while replacing its attempted marker.
When the target marker is absent or malformed, initialize a valid current
attempt with `completed: null`; never reinterpret malformed data as a completed
predecessor. Merge against fresh project state, preserving unrelated properties
and other languages' entries. A project write refused by the existing gate
remains refused; the marker does not provide alternate write authority.

### Re-observation and historical-edge eligibility

Without a fully completed predecessor, every encountered file of that owner
must follow the real source read, parse, persistence and re-resolution path,
even when its content hash, scanner identity and declaration ownership match.
Select recovery before the unchanged-file decision and `preserve_l2_state`.
Existing failures or refusals may still skip a file; recovery does not replace a
read/parse/gate refusal with a refresh. Successful empty declaration sets remain
observed coverage.

Recovery republishes only references actually observed through the existing
scanner and resolution rules. It does not make every failed-run stamp eligible,
revive removed references, refresh manual edges, delete historical rows or
change owner/endpoint authority. On a fully completed predecessor, retain the
unchanged-file fast path and the existing exact predecessor-stamp guard for
natural `depends_on` and `implements` refresh. The inbound `contains` refresh
retains its existing owner/derived predicates; this amendment does not claim it
has the same predecessor-stamp predicate as the natural-edge refresh.

The existing `sweep_clock` and `last_seen_at` keep B5's meaning: they record the
latest invocation time and actual entity observation, respectively. The new
completion marker supplies L2 reuse authority, rather than replacing visible
clocks with a completed-only clock. Owners and languages are independent. No
selected L2 work means no L2 completion state. Reuse authority is owner-wide, so
only an invocation that covered the whole owner may grant it: an invocation
whose ingest `path` lies strictly inside an owner's project root (the directory
of that owner's manifest, compared after the canonicalization ingest already
applies) writes that owner's attempted marker like any L2 invocation and never
its completed marker. The next invocation that covers the whole owner therefore
finds no completed predecessor and recovers, instead of fast-pathing unchanged
files outside the earlier subtree whose edges carry older stamps.

### Graph completion, skips and accounting

For each participating owner whose project root lies inside this invocation's
ingest `path`, write its completed marker only after this invocation's file
work, pending-write flush, synchronous re-resolution, natural unchanged-edge
refresh and inbound containment refresh have all finished. The
completed marker carries this invocation's own run ID and exact sweep-time
string. When the fresh project state's attempted marker for that owner does not
carry this invocation's run ID, the invocation writes no completed marker, and
the entry stays without reuse authority. This is one additional guarded project
mutation after graph work, merged with fresh properties and other languages'
state. It is not a request-wide
transaction: prior graph/entity/FTS writes remain committed when a later
operation fails or the future is cancelled.

Completion means **graph completion of this invocation's observed coverage**.
It does not mean every source file was successfully read or parsed, every gate
accepted, or the response was delivered. Read/parse/gate-skipped files' historical
edges are never promoted. Their existing stale-edge strand after an otherwise
completed invocation remains a known limitation outside this amendment. Missing
files, removed references and unvisited subtree members remain historical.

An owner whose manifests sit under more than one project root is outside the
whole-owner guarantee. An invocation whose `path` covers only one of those roots
can write that owner's completed marker, and the next whole-owner invocation then
takes the unchanged fast path for the other root's files, whose edges keep their
older stamp until a file under that root changes. This is a known limitation,
tracked as #3752.

The entity compare-and-set can commit the completed marker before its following
FTS document write fails. In that case the invocation returns its existing
error while graph completion remains durable. A crash after the completion row
commit and before response acknowledgement has the same durable interpretation.
This does not promise FTS convergence or a successful response, and must not be
reported as a rollback of the completed row. Failure before completion leaves
an incomplete attempt and requires real re-observation on the next sequential
invocation.

Public report fields retain their meanings. The attempted marker shares the
existing project mutation and adds no separate project/FTS accounting. A
successful completion update adds one to `projects_updated`; its successful FTS
write adds one to `fts_indexed`. Existing entity revision/version history also
advances for that additional mutation. No new report field is introduced, and
an invocation returning an error does not acquire a successful report merely
because its completed row is durable. The dependent change must disclose these
counter and mutation changes, the sequential-only guarantee, the durable
completion/FTS-error boundary and the remaining skipped-file strand.

### Acceptance and sequencing

Before dependent recovery code merges, review and ratify this final amendment
through the existing ADR process. Landing Proposed text alone does not adopt it
or release its dependent code. The code change must follow the already frozen
L2 batching change (#3715), preserve its pending-write ordering and failure
behavior, and use a separate private recovery fixture. This amendment neither
ratifies Amendment 13 nor releases that amendment's native-routing hold.

Acceptance must use real file-backed runtimes and actual committed storage
failures. Cover WAL and rollback DELETE for the sequential recovery mechanism;
the WAL fixture does not change the dedicated code-map target's governed journal
posture. Retain an original-source baseline and independently applicable removal
controls. A timeout, compile failure, zero selected tests or an authored fixture
is not an executed proof.

- Reproduce an interrupted early file/persistence run and a partial final refresh
  that leaves both old and failed-run edge stamps. After removing the actual
  SQLite fault, unchanged disk must trigger real parsing and republish every
  still-observed reference. Removed, manual, foreign-owner and unvisited edges
  must retain their historical state. Removing forced reparse must fail the
  mixed-stamp witness.
- Fail after changed-file or re-resolution work has committed, and cancel after
  a real committed edge update. Reopen an independent runtime before recovery;
  observe the committed prefix rather than simulate an error before the write.
  An attempt marker moved after destructive work must fail the corresponding
  recovery control.
- Complete a whole-owner invocation, then complete an invocation whose `path` is a
  subdirectory of that owner, then change nothing and run a whole-owner
  invocation again. The subtree invocation must leave no completed marker, and
  the final invocation must parse for real every file outside the subtree.
  Letting the subtree invocation write its completed marker must fail this arm.
- Exercise missing, malformed, unknown-version, incomplete and clock-mismatched
  new marker entries. Such entries must force real parsing without broadening
  legacy predicates. Repeat and reverse sweep times with distinct run IDs; a
  timestamp-equality completion control must fail. Replace the attempted marker
  with a different valid run ID after this invocation's attempt and before its
  completion: no completed marker is written and the next invocation parses for
  real. A completion that copies the stored attempt must fail this arm.
- Inject a real fault on the completion row, then a separate real fault on its
  post-row FTS write. Assert incomplete recovery in the first case and durable
  graph completion plus the returned FTS error in the second. Completing before
  either refresh phase or pending flush must fail a named witness.
- Observe successful recovery followed by unchanged fast-path reuse, independent
  project/language state, no-L2 calls, successful empty files and skipped files.
  Assert the extra successful completion mutation and report accounting. A
  skipped-file fixture must retain, rather than conceal, the known stale-edge
  limitation.

Ratification releases the dependent recovery change (#3736) to its normal
review. No implementation or native acceptance is asserted by this amendment.

## Amendment 15 (2026-10-03): fallback owners neither grant nor use L2 reuse authority

**Status: Accepted; ratified by the maintainer, 2026-10-03.**

**Clarifies:** Amendment 14's whole-owner rule. **Retains:** Amendment 10's read
boundary, project/module/symbol identities, the Amendment 14 marker shape and
every other Amendment 14 rule. No project property is added.

### The gap

Amendment 14 lets an invocation write an owner's completed marker only when that
owner's project root, the directory of its manifest, lies inside the
invocation's ingest `path`. When no governing manifest is found inside the
ingest root, source ingest assigns the files to an owner named after the ingest
root's basename and uses the ingest root itself as the project root. That
fallback root always lies inside the invocation's `path`, so the whole-owner
test cannot fail for it, and a subtree invocation can reach the same owner as a
whole-owner invocation.

A witness with a single manifest:

```text
/repo/Cargo.toml     [package] name = "src"
/repo/src/alpha.rs
/repo/other.rs
```

A whole `/repo` invocation assigns both files to owner `src` with project root
`/repo`. An invocation whose `path` is `/repo/src` cannot read
`/repo/Cargo.toml`, because Amendment 10 confines manifest reads to the ingest
root. It falls back to the basename `src` and reaches the same project identity,
and the Rust leading-`src/` rule gives `alpha.rs` the same module path under
both invocations. Its fallback root equals its own `path`, so under Amendment 14
alone it would write a completed marker, and the next whole invocation would
take the unchanged-file fast path for `other.rs` while that file's edges carry
older stamps. There is one manifest here, so the multiple-root limitation
(#3752) does not cover this case.

### Decision

An owner obtained through the basename fallback has no project root in
Amendment 14's sense, so no invocation covers it whole. Three rules follow.

1. **No grant.** An invocation writes a fallback owner's attempted marker as
   Amendment 14 requires and never its completed marker.
2. **No use.** An invocation that reaches an owner through the fallback
   captures no reuse authority for that owner, whatever its stored markers say,
   and re-observes every encountered file of that owner. A completed marker
   written by an earlier manifest-governed invocation does not cover files that
   this invocation reaches without that manifest.
3. **Whole invocation.** If any file of an owner is reached through the
   fallback in an invocation, that owner is a fallback owner for the whole
   invocation: rules 1 and 2 apply to all of its files in that invocation,
   including files that the same invocation resolves through a manifest.

The fallback is recorded when the governing-manifest lookup returns no
manifest, and is carried for the rest of that invocation. It is not
inferred afterwards by comparing the project root with the ingest root: for a
fallback owner the two are equal by construction, so that comparison cannot
separate a whole invocation from a subtree one. Owners resolved through a
manifest keep Amendment 14's rule unchanged.

In the witness above, the subtree invocation replaces the owner's attempted
marker with its own run ID and leaves the earlier completed marker in place.
The two run IDs then differ, so the next whole invocation finds no completed
predecessor and re-observes every file.

Rule 2 closes the same collision in the other direction. With a manifest at
`/repo/src/sub/Cargo.toml` declaring package `src` and a loose file
`/repo/src/x.rs`, an invocation whose `path` is `/repo/src/sub` reaches owner
`src` through the manifest, covers its project root and completes. A later
invocation whose `path` is `/repo/src` reaches the same owner through the
fallback for `x.rs`. Without rule 2 it would capture the earlier completion as
reuse authority and take the unchanged-file fast path for `x.rs`, whose edges
carry stamps older than that completion.

The cost is stated rather than hidden. An owner reached through the fallback
never has usable reuse authority, so every L2 invocation for it re-observes every
encountered file through the real read, parse and re-resolution path; such
owners lose the unchanged-file fast path. Durable owner-root provenance, which
could restore that fast path, is not specified here. It would be a separate
amendment, proposed only with a measured cost.

### Acceptance

These arms join Amendment 14's acceptance under the same executed-evidence
rules.

- Run the single-manifest witness: a whole invocation, a subtree invocation,
  then an unchanged whole invocation. The subtree invocation leaves no completed
  marker that matches its attempt, and the final invocation parses every file
  outside the subtree for real. Removing the fallback rule must fail this arm.
- A manifest-governed whole invocation followed by an unchanged whole invocation
  still writes its completed marker and takes the unchanged-file fast path.
- A manifestless whole invocation followed by an unchanged whole invocation
  parses for real both times, asserting the stated cost.
- A manifest-governed subtree invocation completes, then an unchanged
  invocation that reaches the same owner through the fallback parses the
  fallback files for real. Removing rule 2 must fail this arm.

The multiple-root limitation (#3752) remains outside the whole-owner guarantee.
Ratifying this amendment, with Amendment 14, releases the dependent recovery
change (#3736) to its normal review. No implementation or native acceptance is
asserted by this amendment.

## Amendment 16 (2026-10-03): retained declaration and edge observations gate L2 reuse

**Status: Proposed.**

This amendment addresses the remaining manifest-governed case of #3752.

**Amends:** Amendment 14's unchanged-file reuse prerequisite and its deferred
multiple-root and restored-file strands; Amendment 15's retained multiple-root
exclusion and its unconditional whole-invocation fast-path acceptance. **Retains:**
B5 observation semantics, deterministic owner/module/symbol
identities, marker shape, opaque time comparison, sequential-only recovery,
fallback refusal of reuse authority, pending-write ordering, the existing secret
gate and every other rule of Amendments 14 and 15. No stored root provenance,
property, schema, lease, serialization or clock ordering is introduced.

### Superseded deferrals and acceptance

The following accepted text remains recorded above. This amendment replaces the
listed deferrals, narrows Amendment 14's completed-predecessor fast path with the
declaration and edge prerequisite, and replaces the quoted Amendment 15 acceptance
below:

- Amendment 14: “Their existing stale-edge strand after an otherwise completed
  invocation remains a known limitation outside this amendment.” A restored file
  whose retained declaration observations fail the new prerequisite must now
  re-observe its actual source.
- Amendment 14's multiple-root paragraph: “This is a known limitation, tracked as
  #3752.” Manifest-governed roots sharing an owner now obey the same per-file
  observation prerequisite; completion still describes the invocation's observed
  coverage and grants no observation of an unvisited root.
- Amendment 14's acceptance: “A skipped-file fixture must retain, rather than
  conceal, the known stale-edge limitation.” Retain the actual read refusal and
  durable completion assertions, then prove real re-observation of a restored
  file when the prerequisite fails instead of retaining the final stale strand.
- Amendment 15: “The multiple-root limitation (#3752) remains outside the
  whole-owner guarantee.” The prerequisite now governs that remaining case.

Amendment 15 also contains this acceptance requirement:

```text
- A manifest-governed whole invocation followed by an unchanged whole invocation
  still writes its completed marker and takes the unchanged-file fast path.
```

Replace that acceptance with: A manifest-governed whole invocation followed by an
unchanged whole invocation still writes its completed marker. Each unchanged file
takes the unchanged-file fast path only when its retained declaration and outgoing
derived-edge observations satisfy this amendment's prerequisites; otherwise it
re-observes actual source through the existing read, parse and re-resolution path.

### Decision and current-coverage proof

An unchanged file may refresh retained declarations only when every retained row
has a canonical code declaration kind, belongs to the file's existing owner and
language, and carries `last_seen_at` exactly equal to that owner/language's
captured completed predecessor stamp. Check all retained rows before refreshing
any of them. Also require every live outgoing `depends_on` or `implements` edge
from those retained IDs with `l2_derived: true` to carry that exact predecessor
observation. This includes an edge whose target was not visited by another root:
partially shared declaration identities alone cannot prove prior edge coverage.
Other relations, soft-deleted edges and edges without boolean `l2_derived: true`
do not block reuse. A caller-authored edge marked derived follows that same rule;
an ordinary manual edge does not. No target history is made current by the audit.
Compare the existing serialized observations as opaque text; do not
parse, normalize or order timestamps. Recheck declaration kind, owner/language and
observation against the fresh row inside each guarded declaration mutation. After
those mutations, repeat the live edge audit with fresh graph reads before
returning reusable authority. These are separate observations, not a transaction
across entity and edge rows or a guarantee against concurrent owner writers.
A missing declaration or audited edge, invalid predicate or refused refresh
selects the existing actual source
parse/persist/re-resolution path for that file. A storage error retains its
existing error and committed-prefix behavior.

Successful reuse stamps every retained declaration with this invocation's
observation before marking its declarations current and unchanged. Successful
parse stamps each allowed observed declaration and records only those retained
declaration IDs before marking them current. Thus, after one completed invocation,
every retained declaration marked current for its owner/language has this
invocation's stamp, including declarations of reused files. No second repair
ingest is required to establish that coverage. This remains a sequential
guarantee, not authority over overlapping writers.

Filtered, missing, outside-root, unreadable and parse-refused files supply no
current declarations. Their unobserved history remains historical; a later
encounter retries ordinary read/parse checks, and a successful read follows the
new prerequisite or existing missing-coverage reparse rule. A persistent refusal
never becomes observation. Gate-refused declarations and descendants of a
gate-refused inline module remain outside retained current coverage. When a
successful parse records only an allowed subset, an unchanged later invocation
can reuse that subset; clearing the gate refusal alone does not force discovery
of an excluded declaration. A subsequent real parse retries it under existing
gate rules. Gate-refused scaffolding or ownership writes can leave unusable
coverage and force repeated parsing on later invocations. Another unvisited root
does not gain current declarations; its next successful encounter must satisfy
the prerequisite or reparse. Files with successful empty declaration sets remain
valid observed empty coverage.

Natural unchanged-edge refresh keeps **both** existing exact predecessor-stamp
checks, the `l2_derived` checks and current same-owner endpoint authority. A
reparsed file republishes only references the scanner actually observes and
resolves. Removed references, unobserved deleted rows and manually authored edges
remain historical. Inbound `contains` refresh retains its existing predicates.
No historical timestamp range is made eligible.

### Cost and boundaries

Reuse checking applies to every unchanged project file encountered by the
invocation, so refresh work scales with all those files. Successful reuse of a
nonempty retained declaration set performs two outgoing-edge audits, before and
after the guarded declaration writes. Each audit calls `GraphStore::batch_neighbors`
and `GraphStore::get_edges`: four graph capability calls in total. Physical SQLite
SELECT counts depend on the declaration and edge-hydration chunks; an audit with no
matching outgoing edges has no edge-hydration SELECT. Empty declaration sets issue
no graph read, and an earlier declaration refusal or failed first audit prevents
later audits. A successful warm same-root repeat still writes every retained
declaration with that invocation's observation.

With different non-empty retained declaration identities and different sweep-time
strings, each alternating manifest-governed A/B invocation sharing an owner finds
the visited root's declarations different from the completed predecessor and
reparses its encountered files. That cost persists for every such alternation;
the first return alone is insufficient acceptance evidence. One immediately
repeated same-root invocation can reuse the now-current retained declarations.
No measurement of how often shared owners occur in practice is claimed.

Root alternation alone is not an unconditional prohibition on reuse. Empty sets
satisfy the all-declaration predicate vacuously. Identical module/declaration
identities with unchanged bytes can share refreshed declaration and edge rows.
Known gap: reuse eligibility compares a retained observation's sweep-time string
with the completed predecessor's string and does not compare run identity. The
string is the invocation's wall-clock reading in RFC 3339 form with the
fractional digits the reading has (none, three, six or nine), so its precision
is that of the platform clock, down to nanoseconds, not seconds. The production
handler reads the clock once per invocation and applies no per-owner uniqueness.
The trigger is therefore two invocations for one owner and language that read an
identical clock value (the same tick, or a clock stepped back onto an earlier
value): a completed invocation then grants reuse for declarations and natural
edges that it did not observe and that another invocation stamped with the same
string. No such collision has been measured. Making observation run identity the
authority, with rows that carry none reparsed once, is tracked in #3752.
These cases retain the accepted identity, empty-coverage and opaque-time rules;
they do not introduce root provenance. Partially aliased declarations must still
pass both edge audits. A file that retains a removed live derived natural edge
whose historical observation differs from the captured predecessor reparses on every unchanged
ingest that sees that mismatch. This cost is permanent while successive
predecessor stamps keep differing from that historical observation, for every
owner, shared or not. No stored complete reference list exists to distinguish that
history from an unobserved still-present reference. Missing or wrong-type edge
observations likewise refuse reuse. Audit read errors retain the existing storage
error type.

### Acceptance

Use real file-backed WAL and rollback DELETE runtimes and observe actual parser
calls.

- Manifest-governed A/proj/alpha.rs → B/proj/beta.rs → unchanged A/proj/alpha.rs
  must reparse actual references and stamp its natural call and positive impl at
  the new owner stamp without changing identity or evidence. Repeat alternation
  at least twice more; each visited file parses once. Assert every retained
  current declaration after one completed ingest and after warm same-root reuse.
  Removing only the declaration observation prerequisite must fail the old-row
  refusal assertion; removing the complete observation prerequisite must fail the
  natural-edge timestamp assertion. Removing its guarded rebase recheck must fail a separate
  real changed-row witness.
- With identical common.rs callers in both roots but an exclusive alpha.rs target
  in A, A T10/B T20/A T30 must really re-observe the shared caller's cross-file
  reference. Removing both edge audits must fail that natural-call timestamp.
  Change an audited edge after the declaration read pause and before its guarded
  write resumes: the final fresh audit must force real parsing. Removing only
  that final audit must fail the edge timestamp assertion.
- Distinct manifest owners and completed same-root invocations keep reuse.
  Pin empty/aliased/repeated-stamp cost boundaries with actual source files.
  Preserve Amendment 15's real fallback reparse arms.
- Restore a file after an actual read refusal and completed sweep with a
  different stamp: parse actual source and refresh only observed references.
  A removed call, an unobserved deleted edge and a manual edge must remain
  unchanged. Pin the permanent repeated-parse cost of retained removed live
  natural history. Independent direct edge-refresh predecessor-filter and derived-filter controls must
  each fail their corresponding historical-row assertion.
- Preserve Amendment 14's committed-fault/cancellation recovery, mixed-stamp
  re-observation, strict marker and completion/FTS durability acceptance.

## Amendment 17 (2026-11-16): configured production parents resolved once per admission

**Status**: Proposed\
**Amends**: Amendment 12's production-path sampling and parent-resolution rules\
**Tracking**: #3664\
**Retains**: Amendment 9's complete configured production set, Amendment 12's
target no-follow and retained-handle proofs, and the separate #3593 lifetime gap.

### Problem and scope

An operator may configure a production database through a symlinked parent, for
example a home directory redirected to another volume. Issue #3664 records the
refusal produced when the target's no-follow traversal is also applied to that
configured parent, even for a clean map in a separate directory. The configured
parent identifies the objects being protected; it is not the caller's map target.

At revision `cf9ceab64a1854ad0e2d7540f970b1eb289465b1`, `sample_protected` already
canonicalizes each configured parent and pins it for one sampling pass
(`crates/khive-db/src/code_map_vfs/mod.rs`). That pin is not retained across the
whole admission, and the original configured parent is not compared again at its
end. Current code also treats a missing configured parent as an empty sample.

This amendment makes the production-parent resolution exception explicit,
requires the pin across admission and a final comparison, and defines the issue's
"inside the production directory" refusal by direct-parent identity. Those
additional guarantees are proposed behavior, not claims about what ships today.
Amendment 12's accepted text remains in force until this amendment is ratified;
the replacement text below takes effect on ratification.

This amendment does not choose Amendment 13's worker architecture, change
quarantine or registration capacity, or resolve #5614's capacity-isolation
requirement. Amendment 13 remains Proposed with its own sign-off and acceptance.
No configured-root policy or code-pack parameter changes are introduced here.

### Decision

1. **Resolve configured production parents per admission.** For every configured
   production main or event path in Amendment 9, resolve its parent directory,
   then pin the resolved directory before sampling any protected leaf beneath it.
   These are ordered steps: a successful resolution records a present directory;
   a disappearance or unsafe identity change between that resolution and the
   native pin is a failure to pin safely, not an absent-parent observation.

   Resolution may follow a Unix parent symlink. A dangling symlink at any followed
   component is an unsafe resolution and refuses; it is not genuine absence.
   Genuine absence means a missing ordinary path component, with no dangling or
   unsupported redirect responsible for the missing destination. It records no
   pin, protected sample or direct-parent match for that parent. An implementation
   must distinguish these cases rather than equate every follow-resolution
   `NotFound` with absence. If it cannot safely establish which case occurred,
   it refuses. All other unsafe resolution or pin failures also refuse.

   On Windows, the configured-parent exception allows only directory symlink
   and mount-point/junction redirects (`IO_REPARSE_TAG_SYMLINK` and
   `IO_REPARSE_TAG_MOUNT_POINT`); other reparse tags refuse as unsafe. Dangling
   allowed redirects also refuse. Use the same retained-handle identity as
   Amendment 12's current production sampling: `GetFileInformationByHandleEx`
   with `FileIdInfo`, comparing `FILE_ID_INFO.VolumeSerialNumber` and the full
   128-bit `FileId.Identifier` (`code_map_vfs/windows.rs:538–558` at the source
   revision), including its existing unavailable/zero-identity refusal. Do not
   substitute the older file-index fields used by other metadata calls. Directory
   pins use `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE`, as the native
   directory opener already does, so the pin does not turn a rename or removal
   into a sharing violation. Unix pins retain device and inode from the handle.
   The exception does not relax reparse refusal for explicit targets or protected
   leaf names.

   Keep the original configured-parent spelling for the final comparison;
   replacing it with the resolved spelling would miss a retarget of the alias.

2. **Sample through the pin.** Inspect the configured main or event name and its
   presently existing journal, WAL and SHM companions with no-follow metadata
   operations relative to that pinned directory. All samples for that parent
   within the admission use that same pin. Do not reopen the original configured
   pathname to sample a leaf, and do not follow a protected leaf symlink or
   reparse redirect. Existing protected-member readability, identity and change
   checks remain mandatory: an unsafe read or an observed protected-member
   change refuses. Pinning the parent is not permission to accept a changed
   protected member.

3. **Refuse targets sharing a protected direct parent.** Compare the identity of
   the target's pinned direct-parent directory with every pinned configured
   production-parent identity. Equality refuses admission even when the target
   is an unrelated file with a different file identity. The comparison uses
   directory identity, not path spelling or a string prefix. It is not an
   ancestry walk: a distinct subdirectory is not the same direct parent merely
   because it is beneath a production directory. Such a target still must pass
   every other target, protected-member and opened-handle check. Protected file
   aliases refuse independently of this directory rule, including aliases whose
   direct parent is different.

4. **Open, compare, then admit the initial main.** After the preliminary
   protected-member and target checks and the existing pre-open capacity
   reservation, OS-open or create the exact initial target main handle. Only
   then resolve each original configured parent again under Decision 1's rules
   for the final comparison. This order is mandatory: initial main open, final
   configured-parent comparison, then admission to header/content inspection or
   SQLite use. Metadata-only identity, file-type and link-count attestation
   through the retained handle is permitted before the comparison; header/content
   reads, SQLite reads or writes, locking and mapping are not.

   For a parent initially pinned, compare the final retained-handle identity
   with the initial pin: disappearance, unsafe resolution or identity read,
   failure to pin the resolved directory, or unequal identity refuses. Keep the
   initial pins through the comparison. An unsafe or changed configured parent
   has its own typed refusal reason, distinct from a target no-follow,
   direct-parent or protected-member refusal.

   For a genuinely absent parent, genuine absence at the final comparison does
   not itself refuse. A directory that now exists refuses because this admission
   has no initial pin or protected sample for it. Creating a dangling redirect,
   or retargeting an allowed redirect to a missing destination, is unsafe and
   refuses at the final observation too. An initially dangling redirect already
   refused; it is never an absent-state baseline. Do not skip the final comparison
   when no initial pin exists. Every later independent admission resolves anew,
   including after refusal, and pins any parent then present.

   The initial main is already open when the final comparison runs. On a
   prior-WAL transition this is the main handle opened for header inspection.
   A rollback journal or transition-WAL companion opened after main admission is
   not present at this comparison. Those later opens use the same parent
   context, retain Amendment 12's per-open protected-member and retained-handle
   checks, and perform no new configured-parent comparison. Decision 6 also
   retains this context for the transition's rollback reopen.

   Every final-comparison refusal after the required open retains that exact
   opened but unadmitted main descriptor as an unclassified possible production
   alias in Amendment 12's bounded, process-lifetime quarantine. This includes
   a clean target with no sampled protected-identity match: each retained
   descriptor consumes its reserved slot. An OS creation may therefore already
   have changed the target pathname; this is not a no-side-effects guarantee.
   Never close such a descriptor through the ordinary path or a close deferral
   that knows only guarded-map locks. Existing pre-open reservation and cap rules
   remain: exhausted capacity refuses before another OS open. A pre-open refusal
   holding no descriptor consumes no quarantine entry.

   Independently of the ordinary admitted-role ledger, refuse any later target
   whose native file identity matches an unclassified quarantined descriptor,
   for the process lifetime. This rule has no same-role exemption and does not
   depend on the current protected samples or a later assessment that the file
   is clean. It neither retires nor closes the retained descriptor. A match found
   before opening creates no additional retained entry; a match discovered after
   an OS open retains that newly opened descriptor too, under its already
   reserved slot, before any header/content or SQLite use. Do not deduplicate
   these descriptors by closing one. Keep the existing caps, their target-parent
   native-identity plus leaf domain, and restart remedy. Ordinary Amendment 12
   same-role reuse and Unix admitted-ledger retirement remain unchanged for
   identities outside this unclassified-quarantine set. A later successful clean
   admission must use a different identity and still pass every other check.

5. **Keep the target boundary.** An explicit `db` target still receives no
   parent-path canonicalization and every component retains native no-follow
   traversal. An omitted-`db` default retains its existing one-time configured
   parent canonicalization. Permission to resolve a configured production parent
   does not grant permission to canonicalize an explicit target, even when the
   two path spellings share a prefix.

6. **One parent context for a quiescent WAL transition.** The transition retains
   its initial production-parent pins, absent states and protected-name set as one
   admission context. OS-open its initial main handle first, then complete the
   single final original-parent comparison before that handle is admitted to
   header/content inspection or SQLite use, and therefore before the FULL
   checkpoint and `PRAGMA journal_mode=DELETE`. The
   remaining Amendment 12 transition-refusing checks still finish before that
   PRAGMA. No target already aliased to an unsampled production file may be read
   or checkpointed while waiting for this final comparison.

   The header open, later main/WAL attestations, post-checkpoint protected-member
   recheck, post-switch companion check and rollback reopen all retain that same
   production-parent context. Refresh protected leaf samples through its original
   pins and retain the existing member-change checks; do not select a new
   production directory or silently start another configured-parent resolution
   during those steps. Sidecar unlink still uses the separately attested target
   parent, never the production pin. The rollback guard inherits the same
   production-parent context for its reopen proof rather than resolving the
   configured names afresh after DELETE.

   A retarget after the final comparison is outside this guarantee, including the
   intervals before checkpoint, before DELETE, during companion cleanup and
   before rollback reopen. A configured production parent genuinely absent at
   both observations that appears after the final comparison is also outside
   this guarantee throughout those intervals: this context has no pin or sample
   for it until the next independent admission. Later genuine target/companion,
   locking or reopen
   failures keep Amendment 12's incomplete-versus-PARTIAL outcomes: in particular,
   a failure at or after DELETE does not claim intact sidecars or an unchanged
   target. A later independent admission starts with a fresh parent resolution.

The two configured-parent observations do not detect an unobserved ABA retarget,
a retarget after the final comparison, or a parent absent at both observations
that appears afterward. The latter remains unsampled until the next independent
admission. They do not prove the identity of production handles whose names
moved or changed outside the sampled
set. The snapshot remains best-effort; #3593's production-handle lifetime gap
remains open. This change neither replaces the exact retained-handle proof for
the target nor closes a refused possible production alias held in quarantine.

### Exact amendments to Amendment 12

On ratification, replace its production-sampling sentence beginning "At each
admission, obtain a **best-effort path-stat snapshot**" with:

> At each admission, resolve the parent of every configured production main and
> event path in Amendment 9, pin each existing parent and record only genuine missing ordinary parents
> as absent, refusing dangling or unsafe redirects under Amendment 17, then obtain a **best-effort path-stat
> snapshot** of the leaf names beneath the pinned parents and all presently
> existing `-journal`, `-wal` and `-shm` companions through the retained parent pins using no-follow metadata
> reads. Refuse a target whose pinned direct-parent identity equals any of these
> production-parent identities. Reserve capacity and OS-open or create the initial
> target main handle before resolving each original configured parent again for
> the final comparison; admit that handle only after the comparison succeeds.
> Metadata-only retained-handle identity, file-type and link-count attestation
> remains permitted before this comparison; header/content reads, SQLite use,
> locking and mapping do not. The comparison governs this initial main, including
> a transition's header-inspection main. Later journal/WAL opens inherit the same
> parent context and existing per-open checks without a new configured-parent
> comparison. Refuse
> if a pinned parent's identity differs or cannot
> be read safely, or if an initially absent parent now exists or cannot be
> resolved safely; an initially absent parent that remains genuinely absent does
> not itself refuse admission. Every final-parent refusal after the required
> initial main open retains that descriptor, including a clean target, as an
> unclassified possible alias in the bounded quarantine specified by Amendment 17.

The rest of that paragraph, including protected-member-change refusal and its
explicit #3593 limitation, remains unchanged.

Replace the paragraph beginning "An explicit `db` receives" with:

> An explicit `db` receives **no parent-path canonicalization transform** before
> the guarded open. Any symlink/reparse component refuses even if a path-level
> courtesy check would normalize it. An omitted-`db` default may canonicalize its
> configured parent **once** before creating/opening `<path>/.khive/code-map.db`,
> for platform aliases such as macOS `/var/folders`; native no-follow traversal
> and retained-handle proof still follow. Separately, configured production
> parents are resolved per admission and pinned when present, with the final
> present/absent and parent-identity comparison required by Amendment 17. These are
> the only parent-resolution exceptions; neither authorizes canonicalizing an explicit target. Controls must
> fail if the default transform is removed or applied to an explicit `/var/...`
> path, or if production-parent resolution is removed.

In Amendment 12's quarantine paragraph, replace the sentence beginning "On a
protected-identity match" with:

> On a protected-identity match, or when Amendment 17's final configured-parent
> comparison refuses an admission that has opened a target descriptor, the VFS
> refuses before admitting the handle for use and retains that descriptor in the
> bounded, process-lifetime quarantine rather than closing it. The initial main
> in the second case is an unclassified possible production alias even when it
> is clean and has no sampled identity match. Any later target matching such an
> unclassified retained identity refuses for the process lifetime, irrespective
> of role or later protected samples; ordinary same-role reuse does not exempt
> it. If the match is discovered after opening, retain the new descriptor too
> under its pre-reserved slot without use or close; a pre-open match holds no new
> descriptor. On Unix, ordinary close can release same-process
> production locks. Descriptor retention is required on Windows too, without
> assuming that closing an alias releases locks held through another handle.

Replace that paragraph's "This quarantine is solely for the stat-to-open swap"
sentence with:

> This quarantine covers the protected stat-to-open swap and Amendment 17's
> opened-but-unadmitted final-parent refusal, plus opened descriptors refused
> because their identity matches an unclassified quarantined descriptor. Pre-open
> companion, identity or resolution refusals and no-follow open failures that
> hold no descriptor do not consume it.

Replace that paragraph's "Non-adversarial runs must end with quarantine occupancy
zero" sentence with:

> Runs with no protected stat-to-open swap, post-open final-parent refusal or
> post-open match to an unclassified quarantined identity must end with quarantine
> occupancy zero. A clean target refused by the final comparison after the
> mandatory open intentionally consumes a slot; later reassessment does not free
> it. The existing reservation limits and restart remedy bound these refusals too.

In the quiescent-transition paragraph, immediately after "Every
transition-refusing check" through "before `PRAGMA journal_mode=DELETE`", add:

> Amendment 17's configured-parent admission, including its final original-parent
> comparison, runs after the initial main OS-open and finishes before that
> handle is admitted to header/content inspection or SQLite use, hence before
> checkpoint or DELETE. Retain that one
> production-parent context and refresh its protected leaves through the same pins
> for all later attestations, post-checkpoint and post-switch checks, sidecar
> cleanup and rollback reopen; those steps do not begin a new configured-parent
> resolution. A retarget after the final comparison, or a configured production
> parent absent at both observations that appears afterward, is outside its
> guarantee throughout that remainder of the transition. Such an appearing
> parent remains unsampled until the next independent admission. The existing incomplete/PARTIAL
> distinction and attested-target-parent unlink boundary remain unchanged.

All other Amendment 12 text, including its capacity limits, reservation rules,
restart remedy and later partial-transition proofs, remains unchanged. Refreshing
protected leaf samples on an open still applies; within one transition it does
not replace the retained production-parent context.

### Acceptance and implementation sequence

The implementation and its witnesses belong to `khive-db`'s `code_map_vfs` and
the guarded `khive-runtime` code-map route tests. The runtime witness constructs
the guarded runtime directly; it does not require a `khive-pack-code` change.
The specification must receive sign-off before dependent source work begins.
Implementation is then reviewed against this complete contract before seeking
ratification; a documentation commit alone establishes no runtime result.

- **Configured-parent admission (W6).** One guarded `khive-runtime` route
  witness has both arms: a symlinked configured production parent admits a clean
  target in a separate directory, and an unrelated target directly inside the
  resolved production directory refuses with the typed direct-parent-equality
  reason. The explicit targets themselves contain no symlink/reparse component.
  Removing only production-parent resolution fails the clean admission assertion;
  removing direct-parent comparison fails the same witness's typed inside-target
  refusal assertion. Neither arm is satisfied only by a lower-level VFS test.
- **Direct-parent boundary and protected alias (W7).** Use distinct non-symlink
  spellings of the same physical parent that pass the unchanged target path-shape
  checks but remain unequal under the implementation's actual normalized `Path`
  comparison, and under canonicalization if that comparison uses it. Assert
  those unequal comparator operands and equal retained native directory identities
  before admission. A letter-case variant on a case-insensitive volume is a
  candidate only if it satisfies those actual preconditions. Redundant separators
  and an interior `.` that normalize equal do not discriminate. If a platform or
  filesystem offers no qualifying spelling, record this arm as not executed
  there; do not substitute a normalizing spelling. Do not use a `..` spelling
  rejected by the path-shape checks, or an explicit-target symlink, as evidence
  for directory-identity refusal. The target
  is an independent single-link file so no protected-member or sidecar link-count
  refusal masks the direct-parent reason. A distinct subdirectory is not refused
  by direct-parent equality alone. In a separate cross-directory protected-alias
  arm, choose the main-file hard-link route that reaches the typed protected-file
  identity comparison without an earlier no-follow or writable-sidecar link-count
  refusal. The pristine oracle passes only for its named typed reason; removing
  each comparison must fail that own assertion, not an unrelated setup assertion.
  Compare protected main and companion
  presence, bytes, size and modification time across refusals.
- **Pinned sampling and observed changes (W8).** Use two independent
  pin-addressing arms with a barrier after the initial pin and first protected
  sample but before the second sample. For the configured-path arm, D2 initially
  holds hard links to every existing protected member in D1 except the one that
  will be replaced; these unchanged members' link counts are already reflected
  in the first sample. After pinning D1 and taking that sample, create D2's hard
  link to the member to be replaced, retarget the configured alias to D2, and
  rename a new file over that member's name in D1. Adding and then removing the
  old object's link returns its link count to the sampled value. Require D2's
  complete second observations to equal the first samples for every member:
  native identity, link count and any other compared fields, including absence.
  The original pin differs only at the intended replacement and sees its new
  identity. Require that second sample's typed protected-member-change decision.
  A mutant that re-resolves the configured path instead sees the unchanged D2
  observations and must fail that own second-sample assertion.

  For the cached-resolved-pathname arm, prepare a separate replacement directory
  before the first sample, with hard links to each unchanged protected member
  but no link to the member to be replaced. After the first sample, rename
  pinned D1 away and move that replacement directory to D1's old pathname.
  Add its link to the member to be replaced, then rename a new file over the
  member in the moved original directory. This balances the replaced object's
  link count; unchanged members gained their extra links before the sample.
  Assert complete observation equality at the old pathname for every member,
  including identity, link count and every other compared field or absence.
  The retained pin still addresses the moved original directory and differs only
  at the intended replacement, whereas a cached pathname sees the unchanged
  observations. A separate cached-pathname mutant must fail this own second-sample
  assertion. Exercise real native directory rename and file rename-over, not mock
  identity values. On Windows, a sharing violation is fixture failure, not a
  member-change refusal.

  Both pin-addressing arms assert the named production second-sample decision
  before the final-comparison phase; a later parent-change refusal cannot supply
  their oracle. The control removing the member-change check is separate from
  either addressing substitution. For the final-comparison arm, keep all
  original-pin members stable and use a clean single-link target outside both
  directories. Record its OS-open before retargeting the configured alias, then
  require the typed parent-identity-change reason before any header/content or
  SQLite use, with that exact descriptor retained and occupancy increased once.
  No earlier target, member, link-count or direct-parent refusal may mask it.
  Removing the comparison or placing it before that OS-open must fail the own
  reason/order assertion.

  After refusal, a fresh admission resolves the new parent. A **different clean
  file identity** admits with available capacity, while an alias of a newly
  sampled protected file refuses with its own protected-identity reason. Retrying
  the previously quarantined clean identity instead requires the distinct
  unclassified-quarantine-identity refusal, even in the same main role. Removing
  only that lifetime identity denial must fail this own assertion; retention and
  ordinary admitted-role checks remain intact. Caching an old parent context
  across independent admissions separately fails the fresh-parent witness.
  Unobserved ABA and exact production-handle lifetime guarantees are not asserted.
- **Missing, dangling and pin-race parents.** Genuine ordinary-path absence at
  both observations admits a clean target; treating it as unsafe fails that
  admission assertion. Creating that parent between observations instead requires
  the typed parent-appeared refusal. A dangling configured redirect initially
  refuses as unsafe; creating a dangling redirect after initial genuine absence,
  or retargeting an initially present alias to a missing destination, refuses at
  the final observation. A directory resolved as present but removed before its
  pin opens requires the typed unsafe-pin refusal and never becomes an absent
  baseline. Removing these distinctions fails the matching reason assertions.
  In a separate timeline, record genuine absence at both observations, then
  create the configured production parent after the final comparison. That
  appearance is outside this admission's guarantee, including the transition's
  remainder; do not require it to refuse. A later independent admission must
  resolve and sample the new parent and enforce its direct-parent/protected-file
  checks. Skipping that fresh resolution must fail the later observation/refusal
  assertion. A pinned existing parent with absent protected leaf names retains
  the existing absent-leaf rules. On Windows, exercise both allowed directory redirect tags,
  an unsupported tag and directory rename/removal while the pin is held; require
  the intended final observation rather than a sharing-violation substitute.
- **Unclassified-alias retention and Unix lock survival (W9).** Between initial sampling and target
  open, retarget the configured parent from D1 to D2 and swap a clean main target
  to a hard link of a production file in D2 whose connection this process already
  holds open. Repeat with an initially absent configured parent that appears as
  D2. Initial-pin samples remain unchanged and no earlier refusal may mask the
  final parent-changed/parent-appeared reason. The exact opened target descriptor
  is not admitted for header/content inspection or SQLite use and enters
  quarantine once, even though D2's identity was absent from the initial samples.
  Metadata-only retained-handle admission attestation remains permitted. On Unix,
  independent native lock observation and a subsequent same-process production
  write establish that D2's lock survives; closing that unclassified descriptor
  instead of retaining it must fail the lock-survival assertion, not setup. On
  Windows, assert that this exact descriptor is retained and not prematurely
  closed, alongside the existing native lock/alias positives. The close mutant
  must fail that retention assertion; do not require loss of a lock held through
  a different Windows handle. Retain the existing pre-open reservation/cap and
  no-descriptor/no-quarantine controls. The descriptor present at this final
  comparison is the initial main, not a journal or WAL opened later. Restore the
  configured parent to D1 after the first refusal and retry the same hard-linked
  target in the same main role. Current D1 samples and direct-parent checks do
  not match it; the unclassified-quarantine identity must still refuse before
  use. Remove only that lifetime identity denial to falsify its own typed
  refusal assertion. If a match is discovered after opening, assert retention of
  that additional exact descriptor and its reserved-slot charge; a pre-open
  match must add no descriptor. The original retained descriptor remains held
  in either case. Repeat the retention accounting with a clean final-parent
  refusal and up to the existing capacity limits: the next capacity refusal
  occurs before OS-open, without an occupancy increment. A control skipping
  retention only for a clean refusal fails its handle/occupancy assertion.
- **Transition and preserved target guarantees (W9).** A deterministic prior-WAL
  route witness records initial main OS-open, then retargets the configured
  parent before its final comparison and asserts the typed final-parent refusal,
  no target header/content inspection or SQLite-use event and no DELETE PRAGMA;
  metadata-only retained-handle admission attestation remains permitted. The
  ordered trace must show OS-open, comparison, then admission on success; moving
  the comparison before open fails that order assertion. A successful transition
  records exactly one final comparison and the same production pins
  through header admission, checkpoint recheck, post-switch companion attestation,
  target-parent unlink and rollback reopen. Starting another configured-parent
  resolution during those later stages fails that context assertion. Journal
  and WAL opens occurring after main admission use that context and the existing
  per-open checks, not another final comparison. Preserve both the explicit
  retarget-after-final and absent-parent-appears-after-final limitations rather
  than require either excluded race to refuse. Existing genuine post-DELETE
  failures remain PARTIAL. Explicit
  target symlinks/reparse redirects still refuse; removing the omitted-default
  transform and applying it to an explicit target each fail their existing
  assertion. Preserve protected hard-link/sidecar checks, hot-journal recovery,
  DELETE concurrency and zero-`xShm*` invariants. Production-parent resolution is
  neither a default-VFS fallback nor an explicit-target traversal bypass.

Record Unix and Windows results separately; Windows uses the explicitly allowed
parent redirect tags and Amendment 12's exact native identity fields. W9's Unix
close-induced lock-loss control is not a Windows oracle: Windows requires its
own exact-descriptor retention assertion and native lock/alias positives. Every
control must build and reach its own semantic assertion, then pass that same
assertion after exact restoration. A refusal of another typed reason is not a
passing refusal oracle. Shared affected suites run before the controls
and after restoration. Compilation errors, fixture failures and empty selection
do not establish any acceptance item. This amendment records no executed results.
