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

**Status update (2026-10-05)**: On Unix this proposal is superseded by the in-place correction to Amendment 12's admission-ledger paragraph, dated there: each admitted object is pinned through a retained handle and retired at link count zero with every guarded open closed. The sentence quoted below no longer appears in that paragraph and is kept here as history. For Windows the proposal stands as written, Proposed, to be ratified or withdrawn with Amendment 13; until then Windows keeps the process-lifetime guarded file-ID ledger.

**Status**: Proposed; pending ratification with Amendment 13. The accepted Amendment 12 wording remains binding until this clarification is ratified. Upon ratification, it replaces exactly this sentence in Amendment 12's admission-ledger paragraph:

> Refresh the path-stat samples on each new open and after a refusal while retaining the guarded-handle identities for their process lifetime.

The proposed replacement sentence is:

> Refresh the path-stat samples on each new open and after a refusal while retaining each guarded-handle identity until its last actual native handle close.

All other accepted Amendment 12 sentences remain unchanged. The proposed lifetime semantics are: A guarded file identity remains in the live, role-tagged set until the **last native handle close**, rather than for the process lifetime. Each admitted descriptor has a lifetime witness; additional descriptors and duplicated native-handle owners count independently or share an explicit reference count. Closing the first descriptor or completing SQLite's logical `xClose` must not retire an identity still retained by a deferred native close or another owner. Once every native owner has closed, the identity leaves the live set before another admission can classify a recreated file under a reused device/inode or platform file ID. A historical identity alone is not evidence that the new file has the historical role.

Protected-alias quarantine entries never leave during the process lifetime, and the quarantine cap and restart remedy remain unchanged. The configured-production path-stat snapshot is refreshed with the same checks as before. This clarification neither implements nor narrows the exact production-handle registry gap tracked by #3593: a production file moved away from all configured names, or swapped after the path-stat sample while its production handle remains open, can still escape the sampled set.

Acceptance must cover multiple admitted descriptors, duplicated owners and deferred native closes. A remaining native owner must keep its identity live after another owner's logical or native close. After the last native close, recreating a file with a reused identity and a different role must succeed without a stale-role refusal; the existing protected-identity swap refusal must still fail when that protection is removed. Quarantine entries must remain live. This lifetime correction and Amendment 13 must be evaluated in the same document revision.

## Amendment 13 (2026-09-30): native routing for in-process code-map clients

**Status: Proposed; architecture fork unchosen; pending ratification.**

**Amends:** Amendment 12's connection coverage and lock-ownership boundary. **Retains:** its native opened-handle proof, complete configured production deny set, rollback DELETE posture, guarded prior-WAL transition, read-only/no-migration requirements and protected-alias quarantine. The Proposed lifetime clarification above is part of this decision. The configured-production path-stat snapshot and exact production-handle registry gap in #3593 are unchanged. Worker-process isolation, moving code-map work to a separate worker process, remains the recorded retirement path for the inexact in-process set and quarantine; this amendment does not implement that worker. The historical `(F5)` parenthetical retained in Amendment 12 names that retirement path, not a section in Amendment 5 or ADR-135.

### Lock interference and proposed ownership boundary

POSIX record locks belong to a process, not to the descriptor or SQLite connection that acquired them. Closing **any descriptor** for that file can remove that process's locks. A process-owned whole-file `F_UNLCK` can remove another client's locks without a close, and a same-process `F_RDLCK` over a previously write-locked range can replace its lock type. SQLite's default Unix VFS calls `posixUnlock` from `unixUnlock`; when its last shared holder releases, it issues `F_UNLCK` with `l_start = l_len = 0`. A separate native lock table cannot see that default-VFS owner's bookkeeping. These hazards apply on **every Unix host gate, including Linux and macOS**, in both directions between guarded and ordinary clients.

For an in-process design, the proposed boundary is **one native VFS per code-map inode per process**: every in-process open of a governed main or companion file, including same-inode aliases, must route through that owner or return a typed routing refusal before an ordinary/default OS open. A raw-file helper must obtain its proved descriptor and lock/close lifecycle through the same ownership boundary; routing only a later SQLite connection leaves its earlier raw open uncovered. The refusal names the target path and opening path class and preserves that operation's contract; an unrelated blanket refusal is not evidence of safe routing. The rule covers initial and later connections and independently constructed runtimes. It is not currently enforced, and the architecture needed to enforce it remains a decision below.

Ordinary databases retain their VFS and operational behavior. A code-map route must not transfer canonical main/core attachment or GC-liveness authority to a secondary backend. Successfully proved handles must not become process-lifetime quarantine entries to avoid designing their close lifecycle. Amendment 12 confines quarantine to protected-alias stat-to-open races and requires zero occupancy after non-adversarial runs.

A shim sharing SQLite's default Unix VFS inode and deferred-close bookkeeping may address interference between its SQLite clients, but that bookkeeping does not attest the exact retained native handle required by Amendment 12, and it does not cover independent raw descriptors. Such a design requires a new opened-handle proof and the full acceptance population below; a delegating wrapper or reusable-fd lookup alone is insufficient.

### Census and required dispositions

The opening census and executable acceptance population must be the **same population**. The five core classes are **ordinary pool/runtime construction**, **schema/snapshot inspection**, **embedding-model reads**, **showcase map reads**, and **writer attachment**; the first five table rows mark them explicitly. They and the additional known paths below are minimum obligations, not a closed list. The baseline operations below exist at this amendment's landing base; operations introduced only by PR #3674 are labeled separately. Each entry denotes the actual API or helper and all its relevant callers; main files, companions, initial/later opens and same-inode aliases remain included. A source argument that a path usually reaches a different inode, is exported without current callers, or cannot read SQLite-formatted contents does not remove its open/close from acceptance.

| Opening path class and actual operation                                                                                                                                                                                                                                                                                                                                | Required disposition for a governed file or alias                                                                                                                                                                                                                                                                                                                                                      |
| ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **Core — `pool_runtime`**: ordinary `ConnectionPool` and `StorageBackend`/`KhiveRuntime` construction; baseline `khive-db/src/pool.rs::open_standalone_writer_untracked`, `open_standalone_reader`, `open_writer_connection`, the free `open_reader_connection`, and raw `sqlite_header_uses_wal`. PR #3674 introduces the native-aware `open_file_connection` funnel. | Route an authorized map operation through the native constructor, or refuse the ordinary constructor before opening. Include the raw header read as well as initial writers/readers, replacement readers, standalone transactions, writer tasks, checkpoint and diagnostics connections. The baseline ordinary pool/runtime class already exists; its native-aware funnel is introduced only by #3674. |
| **Core — `schema_snapshot`**: `migrations::inspect_schema_version`, `inspect_schema_is_current` and `pool::open_read_only_snapshot_connection`, including `sqlite_header_uses_wal`                                                                                                                                                                                     | Provide an attested native read-only route for both the SQLite connection and raw header helper, or refuse. Preserve snapshot semantics; inspection cannot migrate or silently perform a prior-WAL transition. Include CLI schema and blob preflight callers.                                                                                                                                          |
| **Core — `embedding_models`**: `migrations::query_embedding_models`                                                                                                                                                                                                                                                                                                    | Route the exported read through the native read-only owner, or refuse its map target. A missing current production caller does not establish that this API is unreachable.                                                                                                                                                                                                                             |
| **Core — `showcase_read_map`**: `khive-repo-showcase/src/read.rs::read_map` through `open_read_only`                                                                                                                                                                                                                                                                   | Use an attested native read-only map reader, or refuse. Include export and build callers; a prior-WAL map needs a separate authorized transition. History-store reads retain their ordinary role.                                                                                                                                                                                                      |
| **Core — `writer_attach`**: writable SQL `ATTACH` through `SqlWriter::execute`, scripts and exposed raw/legacy writer connections                                                                                                                                                                                                                                      | Support native attachment with the same identity, protected-set and lock/close guarantees, or refuse before SQLite opens the attachment. Existing pooled-reader `ATTACH` refusal remains. SQL execution can reach attachment even without a literal production `ATTACH DATABASE` string.                                                                                                               |
| `replacement_verify`: `khive-vcs/src/sync.rs::verify_replaced_db`, reached from `run_sync` and the CLI sync command                                                                                                                                                                                                                                                    | Route or refuse the actual verification connection. The usual replacement of the database with a newly built inode is source context, not an executed exclusion of a live governed identity or alias.                                                                                                                                                                                                  |
| `snapshot_copy`: `kkernel/src/code_ingest.rs::open_read_only_snapshot`, used by findings dry-run and entity-type backfill                                                                                                                                                                                                                                              | Route or refuse the original main/WAL/SHM copy opens and closes, as well as subsequent snapshot inspection. Opening only the scratch database through a safe constructor does not cover the source descriptor.                                                                                                                                                                                         |
| `exec_binary_hash`: `khive-pack-exec/src/handlers.rs::hash_tool_binary`                                                                                                                                                                                                                                                                                                | Route its registered-tool path read through the ownership boundary, refuse a governed identity before ordinary open, or move that operation to a worker. No-follow/type checks alone do not exclude a regular-file hard-link alias. Execute the real registration/hash path to establish its reachability or refusal.                                                                                  |
| `code_source`: `khive-pack-code/src/safe_source.rs::open_contained_file` and `read_contained_to_string`, called by manifest and source ingestion                                                                                                                                                                                                                       | Route, refuse or isolate source reads that reach a governed identity. Containment and extension filters are not an identity-based lock boundary; use the actual walker/manifest paths with eligible aliases.                                                                                                                                                                                           |
| Other raw-file operations, including knowledge import/query-set reads and git cache helpers                                                                                                                                                                                                                                                                            | Complete classification and use the selected architecture's route, pre-open refusal or worker disposition. `knowledge/sections.rs::open_import_file_handle`, `knowledge/eval.rs` query-set reads, and git cache staging locks/native directory helpers are known candidates. Directory-only opens and parser rejection require demonstrated dispositions; they are not silently omitted.               |

The implementation review must repeat a whole-tree census of SQLite constructors, writable attachment capabilities, backup/export mechanisms, SQLite CLI/FFI entry points and raw filesystem/native opens. Include a known-positive native-open control in the same search pass, retain match counts and manual production/test classifications, and report residual packs and indirect/imported/macro-generated openers. A zero grep count is not a caller or alias proof. Workspace, git and knowledge must be explicitly accounted for; a partially inspected pack remains residual. An unclassified opener must fail the selected mechanical gate or be confined to the worker boundary. Until that condition is demonstrated, the claim remains limited to the named, executed paths and does not establish arbitrary in-process safety.

### Admission mechanisms still to decide

A first guarded admission must account for an ordinary handle already live on the same native identity. One candidate is enumeration of the process's own descriptors and `fstat`/native identity checks, such as Linux `/proc/self/fd` or macOS descriptor information, combined with a gate serializing **all relevant opens and closes** against admission. Enumeration alone is not atomic, pathname-only registries miss aliases, and an opaque existing descriptor cannot be retroactively relabeled as a proved native owner. If compatible ownership cannot be established, activation must refuse before the new guarded open or use the worker boundary.

Ordinary openers also have a stat-to-open race. An in-process candidate must demonstrate a mechanically enforced admission gate plus retained-native-handle identity proof covering every classified opener and its first use. It must handle substitutions without opening and then closing an incompatible governed alias: that close may already destroy another client's locks. Amendment 12's protected-alias quarantine is not permission to retain every ordinary or successful descriptor. Descriptor enumeration, a proposed gate and a pre-open path check are **unimplemented alternatives**, not claims that this race is closed. If the full process cannot be controlled, worker-process isolation is the alternative.

### Architecture fork and costs

The architecture is **unchosen**. The decision follows the routing-free lock-mechanism evidence and must state the actual scope of its guarantee:

0. **Withdraw the in-process native VFS.** Close PR #3674 without merging and keep Amendment 11's shipped fence as the contract. Amendment 12's opened-handle proof stays specified and unimplemented. The cost is that the gaps Amendment 12 was written to close stay open and must be stated where Amendment 11 states what ships; no guarded client exists, so the guarded-versus-ordinary lock interference described above does not arise, while the pre-existing raw-descriptor hazards on a production identity are unchanged. This option needs no routing, admission gate or worker.
1. **A narrowed in-process boundary plus worker-process isolation for residual raw readers.** Route or refuse SQLite connections and the named raw helpers; move all remaining operations that could open the governed inode into a separate worker. The cost is a maintained caller inventory and cross-process interfaces for residual operations. If a remaining opener can still run beside a guarded handle, the narrowed boundary does not protect that coexistence and cannot support a whole-process guarantee.
2. **An in-process boundary with a mechanical opener gate.** For example, disallow the rusqlite constructor family and raw/native opener APIs outside reviewed, class-tagged admission wrappers. The gate must cover imported aliases, dependencies, indirect calls and new opening paths, with controls proving a newly added unclassified opener fails. A clippy `disallowed-methods` list is only an untested candidate, not an implemented boundary. The cost is broad wrapper/interface changes, complete lifecycle coordination and maintaining enforcement across platforms; a gate covering SQLite alone leaves raw closes outside the claim.
3. **Worker-process isolation as the lock-isolation fix.** Keep code-map descriptors and their lock manager in a dedicated process, so unrelated parent-process opens/unlocks/closes do not share its process-owned locks. The cost is worker lifecycle, IPC, operation/snapshot transfer and failure handling. Every operation that needs a map descriptor must remain inside that boundary. Worker isolation does not remove Amendment 9's protected-file objective or the exact production-handle registry gap by assertion.

No building option can claim closure from green probes of only the five core classes. A proposed OFD primitive must pass reverse-direction controls too: even where its locks survive an unrelated close, closing its descriptor can still erase a same-process ordinary client's classic POSIX locks. Deferring descriptor close is likewise insufficient against whole-file unlock or lock-type replacement.

### Executable acceptance and sequencing

First, a **routing-free lock-mechanism probe** using the pinned bundled SQLite and process-owned `fcntl` locks must reproduce three losses: a second descriptor's close, a default-VFS last-shared whole-file unlock, and a default reader's lock-type downgrade. Each uses one lock per fixture, independent-process `F_GETLK` before and after, and the same fixture without the second client as a lock-survival control. Run on macOS and Linux with fixture files on each test environment's own filesystem. These controls establish the hazard, not a routing implementation or a closed census.

Then review this text and choose the staged architecture decision above. Only after that design decision may a candidate implement its routing, admission and interfaces. **PR #3674's Amendment 12 native-VFS implementation is held and may not merge until this amendment's final acceptance and ratification, below.** The hold is a merge-sequencing statement about that pull request and applies from the time this text lands, whatever the Proposed status of this amendment. The reason is a property of the code, so a decision on paper does not remove it: at the examined #3674 revision, `code.ingest` directly constructs the native code-map runtime; there is no default-OFF rollout switch protecting that route. The ordinary pool's default `code_map_vfs = None` does not disable native-VFS activation in the dedicated code-map constructor. The hold releases only when the executed acceptance of this amendment has passed for the full census population under the selected architecture, the `code.ingest` native constructor included, and the amendment is ratified on that executed evidence; a status change without that executed evidence does not release the hold. No default-OFF merge exception is established here.

Final ratification requires executed evidence for the resulting full population and the lifetime clarification in the same document revision. Production implementation, including PR #3674, remains held under the release condition stated above; a text decision or authored control is not executable lock evidence. This sequencing chooses the design-decision-before-candidate order, **not** one of the architecture options.

For every census entry, invoke its **actual opening path** with direct names and same-inode aliases, all represented initial/later connection classes and a pre-existing ordinary peer. The arm must demonstrate compatible routed ownership, a typed class-specific refusal before an ordinary OS open, or the selected worker disposition. A substituted `File::open`, mock, unselected test, compile failure or narrative about normal inputs cannot prove the caller's behavior. An unreachable claim requires the same executed caller probe, showing no incompatible OS open and unchanged lock state; parser rejection after an open is not pre-open refusal.

Every Unix host gate must include **both directions** for close, whole-file unlock and lock-type replacement. Establish a guarded RESERVED or EXCLUSIVE lock and observe it independently before a default/raw client opens, during its operation, after a completed read transaction and after its close. Observe transaction unlock separately from final descriptor close. Reverse the roles: establish an ordinary/default client's classic lock and prove guarded unlock, downgrade and close cannot erase or weaken it. Use one lock per observation fixture so an observer reporting only the first conflict cannot hide another result. On incompatible-client refusal/isolation arms, prove that actual disposition and retained owner lock; on routed arms, prove the remaining compatible owner's required lock survives. A same-process `F_GETLK` does not report its own process-owned lock as a conflict, and a local lock table is not kernel-lock evidence. The original/unrouted control must reproduce the relevant loss; removing the route, refusal, gate or worker boundary must make the corresponding named runtime arm fail.

Acceptance must also make a newly introduced unclassified SQLite **and raw** opener fail the chosen enforcement boundary. This tests census closure rather than only the paths already listed. If the narrowed option is chosen, the residual worker placement and inability to run those operations beside the in-process guarded owner require executed controls. Concurrent admissions and deterministic barriers around ordinary preflight/open must cover identity substitution and pre-existing peers; a scan without a concurrent-opener arm cannot prove serialization.

Unix and Windows gates retain Amendment 12's native identity/alias refusal, protected-file presence/bytes/size/mtime, prior-WAL transition, hot-journal recovery, DELETE rebase concurrency, retained-owner lifetime and zero-`xShm*` controls. Non-adversarial quarantine occupancy remains zero. Record the exact source revision, named commands and selected tests, OS/filesystem and actual caller/open/unlock/close events, together with independent before/after lock observations. None of the mechanisms, routes, admission alternatives or acceptance obligations here is asserted to have passed merely because this text or its census exists.

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
