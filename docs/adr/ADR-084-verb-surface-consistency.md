# ADR-084: Verb-Surface Consistency Contract and Live Ontology Introspection

**Status**: proposed
**Date**: 2026-07-02
**Authors**: khive maintainers
**Would amend if accepted**: [ADR-023](ADR-023-declarative-pack-format.md) §4 (kg verb
table). The `schema` row is present in ADR-023, where its contract remains proposed
pending this ADR's acceptance and conformance validation (ADR-023 §4, lines 172-200;
crates/khive-pack-kg/src/handler_defs.rs:53-61). Amendment 1 corrects the earlier
implementation and documentation status statements.
**Does not amend**: [ADR-016](ADR-016-request-dsl.md). The DSL grammar requires no change
(see Context §"Phantom gaps").

> **Implementation status (2026-10-23): `schema` shipped.** The verb is registered and
> dispatched as a read operation (crates/khive-pack-kg/src/handler_defs.rs:53-61;
> crates/khive-pack-kg/src/dispatch.rs:150-152;
> crates/khive-gate/src/operation.rs:192). This implementation status does not accept
> this ADR or establish that its other rules and tests are implemented (corrected by
> Amendment 1, Decision 4).

---

## Context

### The disease: documentation drift against a compiled surface

khive's ontology, 9 entity kinds, 20 edge relations, 6 base note kinds (12 declared under
the example packs), and the per-relation endpoint matrix, is a closed, compiled contract
(corrected by Amendment 1, Decision 4). The base endpoint allowlist is defined in
`BASE_ENTITY_ENDPOINT_RULES` (`crates/khive-types/src/edge_rules.rs:37-106`) and re-exported
by the runtime (`crates/khive-runtime/src/operations.rs:432-435`), then extended
additively by each loaded pack's `EDGE_RULES` (ADR-017). The `schema` verb exposes the
merged declarations (`crates/khive-pack-kg/src/schema.rs:52-121`). Agents learn the
contract from documentation snapshots that can become stale:

- **Measured casualty**: a production skill taught 11 relations and 4 entity
  kinds against the then-current contract. The current ontology has 20 relations and
  9 entity kinds (`crates/khive-types/src/edge.rs:81-106;
  crates/khive-pack-kg/src/pack.rs:76-80; corrected by Amendment 1, Decision 4).
  Agents consuming that skill gave wrong answers to endpoint-legality questions until
  the drift was caught by hand.
- **Endpoint-legality questions** ("can a `resource` be the source of an `annotates`
  edge?") previously required reading Rust source or ADRs. The `schema` verb now exposes
  the merged endpoint declarations (`crates/khive-pack-kg/src/schema.rs:74-99;
  corrected by Amendment 1, Decision 4). A recorded wrong-answer incident established
  the instruction to verify edge endpoints against source rules, an instruction that
  concedes the documentation cannot be trusted.

### Phantom capability gaps: the strongest evidence

Three friction items on the motivating list turned out, under verification, to be
capabilities that already ship. Callers believed the surface could not do things it does:

1. **Multi-line strings in the function-call DSL.** Believed impossible; escapes were
   specified in ADR-016 from the start ("String escapes follow JSON: `\\`, `\"`, `\n`,
   `\t`, `\r`") and the parser implements them. Function-call values also accept literal
   newline, carriage-return, and tab bytes, rewriting those bytes before JSON decoding;
   other raw control bytes remain errors (docs/adr/ADR-016-request-dsl.md:136-146;
   crates/khive-request/src/parser/scan.rs:118-218;
   crates/khive-request/src/parser/parser_impl.rs:618-630;
   crates/khive-request/tests/parser_tests/string_values_tests.rs:13-26; corrected by
   Amendment 1, Decision 2). The JSON form keeps strict JSON string rules. The gap is
   discoverability, not grammar.
2. **`memory.remember` tags.** Believed missing; `RememberParams` carries
   `tags: Option<Vec<String>>` and the handler persists them.
3. **`recall | auto_feedback` chaining.** Believed impossible ("`$prev` cannot address a
   bare array result"); `$prev[N].field` addressing is specified in ADR-016 and works,
   including nested inside an array-of-objects argument literal. Verified live:
   `memory.recall(...) | brain.auto_feedback(query="...", results=[{"id":
   $prev[0].id}], target_id=$prev[0].id, signal="implicit_positive")` succeeds. The explicit
   target and signal are required to emit feedback; omission now means abstention (#1588).

The lesson is uniform: when the surface cannot describe itself, agents substitute folklore
for the contract, and folklore decays into false limitations. Prose discipline has not
fixed this and will not.

### Live asymmetries across the verb surface

Independent of drift, the verb surface has accumulated real inconsistencies as packs were
added, each individually small, collectively a memorization tax:

- **Param-name divergence**: `query` vs `q`, `at` (not `due`), `id` (not `thread_id`),
  `ids` (not `slugs`) -- correct names are operational folklore, not a stated convention.
- **ID-resolution divergence**: most `id`-typed params accept a short unique prefix, while
  `brain.feedback` requires a full UUID. `knowledge.get` was another violator at this
  snapshot; its later fix preserves the Rule 1 order: full UUID, exact registered slug,
  then short unique prefix.
- **Silent enum coercion**: `parse_direction` (`khive-pack-kg/src/handlers/common.rs`)
  maps any unrecognized `direction` value to `Direction::Out` (`Some(_) => Direction::Out`)
  instead of erroring -- a direct violation of the codebase's own "never silently coerce
  invalid input" rule. The same function defaults `None` to `Out` while help text has
  advertised `both`.
- **Substrate-asymmetric field names**: entity `tags` is a top-level column while note
  tags live in `properties["tags"]`, so `update(id, tags=[...])` behaves differently by
  substrate; entity `create` takes `description` while note `create` requires `content`,
  and the error on the wrong field does not name the right one.

Each new pack that ships without a stated contract is a fresh opportunity for the same
divergence. Several instances above are already filed as issues; fixing them individually
does not prevent instance N+1.

## Proposed decision

One contract, one introspection verb. The contract states the invariants every pack's
verb surface must obey; the `schema` verb is the contract's runtime projection -- the
machine-readable source of truth that makes the ontology legible to agents and makes
documentation generatable instead of hand-maintained.

### 1. The consistency contract

Normative rules binding all packs compiled into the kkernel binary. Each rule names its
current violators. Existing non-conforming pairs are listed in the Phase-1 legacy
baseline, with one tracking issue per entry. The baseline may only shrink: no entry may
be added, and an entry leaves when its issue closes (Amendment 1, Decision 1).

**Rule 1 -- ID-resolution ladder.** Every parameter that accepts a record identifier
resolves, in order: full UUID → registered slug (where the verb declares slug support) →
short unique hex prefix. A prefix that matches zero or multiple records is an error naming
the ambiguity. Violators at ratification: `brain.feedback` (full UUID only),
`knowledge.get` (slug or full UUID, no prefix). The latter was resolved after this proposal's
snapshot; it now implements this order while keeping UUID/prefix reads namespace-agnostic.

**Rule 2 -- no silent enum coercion.** A parameter with a closed value set rejects
unrecognized values with an error listing the valid values. Defaulting applies only to
_absent_ parameters, never to _invalid_ ones. Violator at ratification: `parse_direction`
(invalid `direction` coerces to `Out`).

**Rule 3 -- help-schema fidelity.** The `help=true` envelope for a verb is generated from
the same `ParamDef` slice the registry serves; a documented default must be the default
the code applies. (`neighbors` was the motivating instance: the proposal snapshot's help
said `both` while its code applied `Out`; its current handler and help both use `Both`
(crates/khive-pack-kg/src/handlers/graph.rs:52-56;
crates/khive-pack-kg/src/handler_defs.rs:1125-1129; corrected by Amendment 1,
Decision 4).)

**Rule 4 -- param-naming conventions.** Canonical names for recurring concepts: `query`
(never `q`), `id` (never `thread_id` or verb-specific synonyms for the primary record
identifier), `ids` (never `slugs` for plural identifiers), `at` (never `due` for
schedule datetimes). This rule binds **new verbs at review time**. Renaming a shipped
parameter is a breaking change to a public surface; existing non-conforming params are
migrated only through an explicit deprecation path -- accept both names for a stated
window, warn on the deprecated one, remove in a versioned release. No silent renames.
Whether any existing param is worth that migration cost is decided case by case at
explicit approval, not by this contract.

**Rule 5 -- substrate-symmetric field names.** Where a concept is identical across
substrates, the parameter name is identical: `tags` means tags for entities and notes
alike (see §3). Where fields are genuinely different concepts -- entity `description`
(metadata about a named thing) vs note `content` (the body that IS the record) -- the
names stay distinct, and the error for supplying the wrong one names the right one for
that substrate.

**Rule 6 -- declared-vocabulary completeness.** A pack MUST declare, through the ADR-017
vocabulary mechanism (`NOTE_KINDS` / `ENTITY_KINDS`), every note kind and entity kind it
writes to the store -- except kinds already declared by a pack it `REQUIRES` (e.g.
`knowledge.learn` writes the kg-declared `concept`; kg owns that declaration). An
undeclared written kind makes the `schema` verb under-describe the live data,
reintroducing one layer down exactly the drift this ADR exists to kill. Audited at
ratification: comm declares `message`, schedule declares `scheduled_event`, and session
declares `session` in their `Pack` impls; brain writes kg-owned `artifact` (covered by
the `REQUIRES` exception) -- the default pack set has no violator. Enforcement: review-level for new
packs, plus the end-to-end smoke test asserting that every kind present in a driven store
run appears in the merged declared vocabulary (`HandlerDef` carries no write-set, so a
static phase-1 check is impossible; the end-to-end smoke test remains required as
specified in this rule and §4 (corrected by Amendment 1, Decision 3).

The Rule 6 end-to-end smoke test is normative and is part of the conformance checklist
for issue [#5557](https://github.com/ohdearquant/khive/issues/5557) (Amendment 1,
Decision 3).

### 2. The `schema` verb

The kg-pack `schema` verb is a bare name, per ADR-023 naming rules, and is assertive,
read-only, and has no side effects. It returns the merged live ontology of the running
binary (`crates/khive-pack-kg/src/handler_defs.rs:53-61;
crates/khive-pack-kg/src/schema.rs:52-121; corrected by Amendment 1, Decision 4):

```json
{
  "entity_kinds": [
    "concept",
    "document",
    "dataset",
    "project",
    "person",
    "org",
    "artifact",
    "service",
    "resource"
  ],
  "note_kinds": [
    "observation",
    "insight",
    "question",
    "decision",
    "reference",
    "head",
    "task",
    "memory",
    "message",
    "channel_health",
    "scheduled_event",
    "session"
  ],
  "edge_relations": [
    "contains",
    "part_of",
    "instance_of",
    "links_to",
    "located_in",
    "extends",
    "variant_of",
    "introduced_by",
    "supersedes",
    "derived_from",
    "precedes",
    "depends_on",
    "enables",
    "implements",
    "competes_with",
    "composed_with",
    "annotates",
    "supports",
    "refutes",
    "owns"
  ],
  "endpoint_rules": [
    { "source": "concept", "relation": "extends", "target": "concept", "origin": "base" },
    { "source": "*", "relation": "instance_of", "target": "concept", "origin": "base" },
    { "source": "task", "relation": "depends_on", "target": "task", "origin": "pack:gtd" }
  ],
  "packs_loaded": ["kg", "gtd", "memory", "brain", "comm", "schedule", "knowledge", "session"],
  "contract_version": "<schema content hash>"
}
```

The JSON above is an illustrative snapshot of the default pack set at ratification, not
a normative fixture: the generated documentation (§5) is the always-current projection,
and prose examples must never be read as racing the binary.

Normative details:

1. **Merged pack vocabulary, not the base enum.** `entity_kinds` and `note_kinds` are the
   union of all loaded packs' declared vocabulary (ADR-017) -- this is how `resource`
   (pack-declared, ADR-048) appears, and how `message` / `scheduled_event` (comm /
   schedule) appear. Rule 6 is what makes this union complete: the verb serializes
   declarations, so an undeclared written kind would be invisible here. Serializing only
   the base `EntityKind` enum would reintroduce the drift this verb exists to kill.
2. **Every endpoint rule carries `origin`**: `"base"` for `BASE_ENTITY_ENDPOINT_RULES`
   rows, `"pack:NAME"` for rows contributed by a pack's `EDGE_RULES`. Agents see exactly
   what loading a pack added.
3. **Wildcards are emitted verbatim.** The `"*"` source of `instance_of` is returned as
   `"*"` with its meaning documented in the verb's help text; it is never expanded into
   per-kind rows.
4. **No filter parameters in v1.** The full matrix is on the order of 150 rows and
   returns as one compact response; callers filter client-side. Server-side filters
   (`schema(relation=...)`) are deferred until measured token cost justifies them.
5. **Presentation**: ADR-045 rules apply. The response must render sanely in agent mode
   (count summary plus full matrix) and `format=table` must work.
6. **Implementation is serialization plus one attributed accessor**: the base matrix is
   exposed via `base_entity_endpoint_rules()` (added for the ADR-076 certificate tests).
   The registry captures pack ownership with each endpoint declaration, and the schema
   handler serializes those attributed rows through `all_edge_rules_with_packs()`
   (`crates/khive-runtime/src/pack/catalog.rs:482-493;
   crates/khive-runtime/src/pack/builder.rs:636-650,961-963;
   crates/khive-pack-kg/src/schema.rs:74-99; corrected by Amendment 1, Decision 4).

The name `schema` aligns with the existing "KG schema.yaml" ontology-manifest concept;
this verb is that manifest's runtime dual. `verbs` and `help=true` are unchanged and
non-overlapping: they describe the verb surface (which verbs, which params); `schema`
describes the data model (which kinds, which relations, which endpoint triples).

### 3. First conformance applications

**3a. Note tags (Rule 5).** `create` and `update` accept `tags` for notes exactly as for
entities, mapped transparently to `properties["tags"]`. Storage is unchanged -- no
migration, no new column. This mirrors a shipped, trusted precedent: `memory.remember`
already maps a top-level `tags` param onto `properties["tags"]`, and `memory.recall`
already filters on it. **Declared divergence**: entity tags live in a top-level column,
note tags in properties JSON; search and index behavior over the two MAY differ. The
surface contract guarantees symmetric _write and read verbs_, not identical _ranking
behavior_. A storage-level unification (top-level note tags column, backfill, FTS trigger
changes) is explicitly out of scope and deferred; its trigger condition is "note-tag
search must rank identically to entity-tag search," which no current consumer requires.

**3b. Create-field errors (Rule 5).** Entity `description` and note `content` remain
distinct fields (they are different concepts). Supplying `content` to an entity create,
or `description`/omitting `content` on a note create, returns an error that names the
correct field for that substrate.
This behavior remains normative and is part of the conformance checklist for issue
[#5557](https://github.com/ohdearquant/khive/issues/5557) (Amendment 1, Decision 3).

**3c. DSL string-error discoverability.** A parse error for an unsupported raw control
byte inside a quoted string must teach the fix: state that string escapes follow JSON, show
the escape form (`\n`), and note that callers passing `ops` through a JSON transport
(every MCP client) must double-escape (`\\n` in the wire form) because the transport
decodes one level before the DSL parser runs (superseded by Amendment 1, Decision 2).
The accepted multi-line behavior is documented in ADR-016, whose parser accepts literal
newline, carriage-return, and tab bytes inside quoted values (docs/adr/ADR-016-request-dsl.md:136-146;
crates/khive-request/src/parser/scan.rs:118-218;
crates/khive-request/src/parser/parser_impl.rs:618-630; corrected by Amendment 1,
Decision 2). Section 5 concerns taxonomy generation, not DSL documentation.

**3d. Tracked conformance failures (Rules 1-3).** `brain.feedback` full-UUID-only,
`knowledge.get` missing prefix resolution, `parse_direction` silent coercion, and the
`neighbors` help/default mismatch were recorded as conformance issues at ratification.
`knowledge.get` has since been resolved with full UUID → exact scoped slug → unique prefix
precedence. The listed resolution, enum, and help/default behaviors are now present in the
code paths (crates/khive-pack-brain/src/handlers.rs:767-788;
crates/khive-pack-knowledge/src/knowledge/crud.rs:861-887;
crates/khive-pack-kg/src/handlers/common.rs:809-817;
crates/khive-pack-kg/src/handlers/graph.rs:52-56;
crates/khive-pack-kg/src/handler_defs.rs:1125-1129), corrected by Amendment 1,
Decision 4. Their presence does not replace the normative conformance checks below.

### 4. The conformance test -- two declared phases

A workspace test walks every registered pack's `HandlerDef` slices and asserts the
mechanically checkable subset of the contract.

**Phase 1 (ships with this ADR's implementation).** Checkable from `ParamDef`
(`{name, param_type, required, description, resolution_mode}`). `param_type` uses a closed
vocabulary to build the published JSON Schema; it does not validate argument values
(`crates/khive-types/src/pack.rs:156-179; corrected by Amendment 1, Decision 3):

- no duplicate param names within a verb;
- param names match the naming conventions (Rule 4), checked against an explicit
  **legacy baseline**: the conformance test carries a checked-in allowlist of the
  non-conforming `(verb, param)` pairs that exist at ratification (`HandlerDef` carries
  no introduced-version marker, so "new" is defined by absence from that baseline). A
  new verb is anything not in the baseline and is checked unconditionally; the baseline
  may only shrink -- adding to it fails the test by construction, which is what makes
  the recurrence guard mechanical rather than review-dependent;
- params whose `param_type` is `"uuid"` (or whose name is `id`/`ids`) follow the
  canonical naming;
- every verb has a non-empty description; required flags are internally consistent.

The Phase-1 workspace test is normative and is part of the conformance checklist for
issue [#5557](https://github.com/ohdearquant/khive/issues/5557) (Amendment 1, Decision 3).

**Phase 2 (declared, not shipped here).** Rules 1 and 2 are _behavioral_: prefix
resolution and enum rejection require execution checks. `ParamDef` now carries an
`IdResolutionMode`, but it has no typed enum-value declaration for validating accepted
values (`crates/khive-types/src/pack.rs:166-179; corrected by Amendment 1, Decision 3).
Behavioral checks for Rules 1 and 2 remain required; this ADR does not claim they are
mechanically enforced by the Phase-1 metadata test.

**Honest boundary.** The conformance test binds packs compiled into the tested workspace.
It does not and cannot bind external third-party packs; for those, the contract is a
published standard plus the `schema`/`verbs` introspection that makes deviations visible.

### 5. Documentation is generated, not maintained

The anti-drift mechanism for docs and skills is generation from the runtime, not prose
discipline:

1. The `schema` verb output is the source of truth for taxonomy documentation.
2. The taxonomy sections of `AGENTS.md` (entity kinds, note kinds, edge relations,
   endpoint matrix) and downstream skills are generated from `schema` output.
3. CI gains a check that the committed taxonomy sections match the live `schema` output
   of the built binary; drift fails the build instead of an agent.

This closes the class of failure the measured casualty exemplifies: a hand-written
taxonomy cannot silently diverge from the compiled one.

## Rationale

- **The contract and the verb are one concern.** The contract states the invariants; the
  verb is how agents (and CI) observe them. Shipping the contract without the verb leaves
  it another prose document that rots -- the exact disease. Shipping the verb without the
  contract exposes an ontology with no stated invariants about the surface serving it.
- **Phantom gaps prove the mechanism.** Three of the motivating frictions dissolved under
  verification because the shipped surface outran its documentation. No grammar change,
  no new capability -- only legibility -- would have prevented all three.
- **Prevention over case-fixes.** The individually filed asymmetry fixes are necessary
  but do not stop pack N+1 from diverging again. The contract plus the phase-1
  conformance test is the recurrence guard.
- **Breaking-change honesty.** Param renames on a shipped public surface break external
  callers. Rule 4 therefore binds new verbs and prescribes a deprecation path, rather
  than pretending existing names can be silently fixed.

## Consequences

**Positive**

- Agents query the live ontology in one call instead of trusting stale snapshots;
  endpoint-legality answers come from the binary, not folklore.
- Taxonomy documentation becomes a build artifact; the kg-digest class of drift fails CI.
- New packs have a stated surface contract and a mechanical gate for its checkable subset.
- The recorded asymmetries stop being folklore and become a tracked, finite worklist.

**Negative / accepted costs**

- One more kg verb to document and maintain; mitigated by it being a
  serialization of existing structures plus one pack-attributed rule accessor (§2.6).
- Phase-1 conformance is name-level only; behavioral rules remain review-enforced until
  the `ParamKind` enrichment lands. This is stated, not hidden.
- The generated-docs pipeline adds a CI dependency on a built binary.

## Alternatives considered

1. **Extend `verbs` with `taxonomy=true` instead of a new verb.** Rejected: conflates the
   API surface with the data model; `help` already means "this verb's params," and
   overloading discovery verbs with ontology data muddies both.
2. **`help` on a kind or relation name.** Rejected: hijacks verb-scoped `help` semantics.
3. **Expose the ontology as a queryable meta-graph (GQL over kind/relation nodes).**
   Rejected: the ontology is ~150 static rows; a second query surface is unjustified.
4. **Fix asymmetries individually with no contract.** Rejected: guarantees recurrence in
   the next pack; several fixes are already filed and this ADR does not replace them, it
   prevents their successors.
5. **Storage-level note-tags unification (top-level column + backfill).** Deferred, not
   rejected: touches FTS/index triggers and every note read path for a problem the
   surface mapping solves at zero storage cost. Trigger condition stated in §3a.
6. **Amend ADR-016 for multi-line strings.** Rejected: the escapes already ship and are
   specified, so no grammar addition is needed. A variant allowing arbitrary raw control
   bytes inside quoted strings was rejected because it would extend the accepted input
   beyond the three-byte newline, carriage-return, and tab carve-out in ADR-016; other
   raw control bytes remain errors (docs/adr/ADR-016-request-dsl.md:136-146;
   crates/khive-request/src/parser/scan.rs:118-218;
   crates/khive-request/src/parser/parser_impl.rs:618-630; corrected by Amendment 1,
   Decision 2). The transport double-escape instruction in §3c is superseded by that
   accepted carve-out.

## References

- [ADR-016](ADR-016-request-dsl.md) -- request DSL; string escapes and `$prev[N].field`
  addressing. The accepted string contract includes the literal newline, carriage-return,
  and tab carve-out recorded in #957 (docs/adr/ADR-016-request-dsl.md:136-146; corrected
  by Amendment 1, Decision 2).
- [ADR-017](ADR-017-pack-standard.md) -- Pack trait, `EDGE_RULES`, pack-extensible
  endpoints (the merge semantics `schema` serializes)
- [ADR-023](ADR-023-declarative-pack-format.md) -- verb surface and naming rules; §4 has
  the `schema` row with a proposed contract pending acceptance and conformance validation
  (lines 172-200; corrected by Amendment 1, Decision 4).
- [ADR-045](ADR-045-verb-response-presentation.md) -- presentation rules applied to `schema`
  output
- [ADR-048](ADR-048-knowledge-section-profiles.md) -- `resource` kind (pack-declared;
  why §2 serializes merged vocabulary)
- [ADR-076](ADR-076-relation-calculability-and-system-role.md) -- certificate tests that motivated
  `base_entity_endpoint_rules()`
- `crates/khive-types/src/edge_rules.rs` -- defines `BASE_ENTITY_ENDPOINT_RULES` at line 37;
  the runtime re-exports it in `crates/khive-runtime/src/operations.rs:432-435`
- `crates/khive-types/src/pack.rs` -- `ParamDef` / `HandlerDef` (phase-1 test substrate)

## Proposed implementation clarification (2026-10-10)

This clarification remains **Proposed**. Source preparation for `schema` does not
accept this ADR or establish completed conformance validation. The status banner above
records the implemented schema verb, while architectural acceptance and conformance
validation remain pending (corrected by Amendment 1, Decision 4). This section clarifies §2 only; it does not implement the other verb-surface or
generated-document rules in this ADR.

`schema()` accepts no filters. Normal registry Gate admission, namespace argument
validation and configured dispatch audit behavior apply. The handler projects only
the loaded registry's declarations; it performs no graph read, graph write, model
initialization or network lookup. An explicit namespace affects admission, not the
metadata selected. The operation is an explicit `Read` under ADR-129 Amendment 3,
which does not exempt it from ordinary incidental audit behavior.

The response retains `entity_kinds`, `note_kinds`, `edge_relations`,
`endpoint_rules`, `packs_loaded` and `contract_version`. A `counts` object contains
the length of each of the first five arrays. The closed relation array is drawn
from `EdgeRelation::ALL`; its current declaration contains 20 names
(`crates/khive-types/src/edge.rs:81-106; corrected by Amendment 1, Decision 4).
Kind and pack-name arrays are lexicographically sorted and deduplicated; relation
spellings are lexicographically sorted.

Endpoint rows retain string `source`, `relation`, `target` and `origin` fields.
`source_substrate` and `target_substrate` are each `entity` or `note`.
`source_entity_type` and `target_entity_type` are omitted when no subtype is
specified. For `EntityOfType`, the kind stays in `source` or `target` and the exact
subtype appears in the corresponding optional field; no joined string syntax or
lossy flattening is introduced. Base rows have origin `base`, both substrates
`entity`, and no subtype fields. Their literal `*` means any entity kind, not any
substrate. Pack rows have origin `pack:<name>` from captured registry attribution.

Rows contain exactly the base entity endpoint table and installed pack endpoint
declarations. They do not replace link validation, including live-endpoint,
self-loop, direction and other semantic checks. Sort rows by
`(origin, source_substrate, source, source_entity_type, relation, target_substrate,
target, target_entity_type)`, with absent subtypes before present ones. Retain all
duplicate declarations, including repeated rows from one owner.

`contract_version` is lowercase 64-character BLAKE3 hex over the canonical JSON
payload excluding only `contract_version` itself, including counts and empty
arrays. Canonicalization uses `khive_types::canonical_json_bytes`: recursive object
key sorting with array order and scalar values preserved, not RFC 8785 numeric or
Unicode normalization. This is a content version, not an authorization signature.

The registered schema handler uses the existing `AlwaysVerbose` presentation
policy so an Agent request retains the complete hash-bearing payload, including
empty vocabulary arrays. The existing table renderer emits every endpoint row and
all vocabulary, pack, count and hash siblings; it does not filter the matrix or
substitute a shorter summary. Table escaping remains display behavior. No new
presentation policy or format is introduced.

## Amendment 1 (2026-10-23) -- Shipped surface and conformance scope

**Status**: Accepted 2026-10-23. It does not change the status of the base ADR or of
earlier amendments.

**Addresses**: [#5557](https://github.com/ohdearquant/khive/issues/5557).

**Trigger.** The status banner and context described `schema` as unshipped and said no
runtime surface exposed the merged contract, while the handler is registered, dispatched,
and classified as a read (`crates/khive-pack-kg/src/handler_defs.rs:53-61;
crates/khive-pack-kg/src/dispatch.rs:150-152; crates/khive-gate/src/operation.rs:192;
crates/khive-pack-kg/src/schema.rs:52-121). The record also conflicts with the accepted
DSL rule for literal newline, carriage-return, and tab bytes inside quoted values
(`docs/adr/ADR-016-request-dsl.md:136-146`). The decisions below state the conformance
baseline, preserve three normative checks, and correct stale counts and implementation
references.

### Decision 1 -- Shrink-only legacy baseline

The Phase-1 legacy baseline is a checked-in allowlist of the existing non-conforming
`(verb, param)` pairs. Each entry names its tracking issue. No entry may be added, and an
entry leaves the baseline when its issue closes. This preserves the explicit deprecation
path for shipped parameter names in Rule 4 while preventing new exceptions. The
no-grandfathering alternative was rejected because treating existing pairs as untracked
failures would block the record on unrelated migrations
(`docs/adr/ADR-084-verb-surface-consistency.md:124-132,
284-303`).

### Decision 2 -- ADR-016 string behavior

The accepted ADR-016 contract governs quoted DSL values. Literal newline, carriage-return,
and tab bytes are rewritten before JSON decoding; other raw control bytes remain errors
(`docs/adr/ADR-016-request-dsl.md:136-146;
crates/khive-request/src/parser/scan.rs:118-218;
crates/khive-request/src/parser/parser_impl.rs:618-630;
crates/khive-request/tests/parser_tests/string_values_tests.rs:13-26;
crates/khive-request/tests/parser_tests/control_characters_tests.rs:16-29). The transport
double-escape instruction in §3c is superseded. The rejected alternative was accepting
arbitrary raw control bytes, which is broader than ADR-016's three-byte carve-out. This
follows the accepted ADR rather than changing it to match a proposed rule; the parser
change that shipped the carve-out is recorded in #957.

### Decision 3 -- Normative conformance checks

The Phase-1 workspace test, the Rule 6 end-to-end smoke test, and the §3b entity-create
field error remain normative and must appear on the conformance checklist for #5557.
The Phase-1 test checks the metadata contract in §4. The Rule 6 smoke test compares kinds
written by its driven store with the merged declared vocabulary. The entity-create check
requires `content` supplied to an entity create to return an error naming `description`.
These requirements retain the scope already stated in §1, §3b, and §4.

### Decision 4 -- Corrected counts and implementation facts

The relation count is 20 (`crates/khive-types/src/edge.rs:81-106`). The base note
vocabulary contains six kinds (`crates/khive-pack-kg/src/vocab.rs:95-131`). The example
packs declare six additional note kinds, for twelve total: comm declares `message` and
`channel_health`, and gtd, memory, schedule, and session each declare one
(`crates/khive-pack-comm/src/pack.rs:40;
crates/khive-pack-gtd/src/pack.rs:25;
crates/khive-pack-memory/src/pack.rs:76;
crates/khive-pack-schedule/src/pack.rs:24;
crates/khive-pack-session/src/pack.rs:29). The nine entity kinds remain as declared by
kg (`crates/khive-pack-kg/src/pack.rs:76-80`).

The `schema` handler is implemented and registered, but ADR-084 remains Proposed until
accepted. ADR-023 places its verb table in §4 and marks the `schema` row's contract
proposed pending ADR-084 acceptance and conformance validation
(`docs/adr/ADR-023-declarative-pack-format.md:172-200`). The base endpoint table is defined
in `crates/khive-types/src/edge_rules.rs:37-106`; the runtime re-exports it at
`crates/khive-runtime/src/operations.rs:432-435`. The schema handler uses the registry's
pack-attributed accessor (`crates/khive-runtime/src/pack/catalog.rs:482-493;
crates/khive-runtime/src/pack/builder.rs:636-650,961-963;
crates/khive-pack-kg/src/schema.rs:74-99).

### Acceptance

| ID | Statement                                                                                  | Evidence                                                                                                                                                                                                                                                                                                                                                                                        |
| -- | ------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A1 | A Phase-1 test checks all compiled handler metadata and permits only a shrinking baseline. | Required workspace Phase-1 `HandlerDef` conformance test in §4.                                                                                                                                                                                                                                                                                                                                 |
| A2 | The Rule 6 smoke test compares every written kind with schema vocabulary.                  | Required extension of `crates/khive-mcp/tests/integration_tests/kind_hook_tests.rs`.                                                                                                                                                                                                                                                                                                            |
| A3 | Singleton entity create with `content` fails and names `description`.                      | Required assertion in `crates/khive-pack-kg/src/handlers/tests.rs`; parameter and branch path in `crates/khive-pack-kg/src/handlers/params.rs:43-54` and `crates/khive-pack-kg/src/handlers/create.rs:788-825,1032-1072`.                                                                                                                                                                       |
| A4 | Schema returns merged declarations, counts, attributed rows, and hash.                     | `crates/kkernel/tests/schema.rs::default_schema_is_complete_attributed_and_does_not_write_domain_rows`; `crates/khive-pack-kg/src/schema.rs:52-121`.                                                                                                                                                                                                                                            |
| A5 | Relation and note-kind counts match their declarations.                                    | `crates/khive-types/src/edge.rs:81-106`; `crates/khive-pack-kg/src/vocab.rs:95-131`; pack declarations in Decision 4.                                                                                                                                                                                                                                                                           |
| A6 | Literal newline, carriage-return, and tab bytes round-trip; other raw controls fail.       | `crates/khive-request/tests/parser_tests/string_values_tests.rs::multiline_string_value_round_trips`; `tab_and_carriage_return_literal_in_string`; `crates/khive-request/tests/parser_tests/control_characters_tests.rs::other_raw_control_chars_in_quoted_string_still_rejected`; `crates/khive-request/src/parser/scan.rs:118-218`; `crates/khive-request/src/parser/parser_impl.rs:618-630`. |
| A7 | The three normative checks in Decision 3 appear on the #5557 checklist.                    | [#5557](https://github.com/ohdearquant/khive/issues/5557).                                                                                                                                                                                                                                                                                                                                      |
