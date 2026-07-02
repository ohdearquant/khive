# ADR-084: Verb-Surface Consistency Contract and Live Ontology Introspection

**Status**: proposed
**Date**: 2026-07-02
**Authors**: lambda:khive
**Amends**: [ADR-023](ADR-023-declarative-pack-format.md) §3 (kg verb table -- adds the
`schema` verb, 16 → 17). The table edit itself rides with the PR that ships the verb, so
ADR-023 keeps describing the live surface at every commit. `AGENTS.md` taxonomy sections
become generated artifacts under §5 of this document once the verb ships.
**Does not amend**: [ADR-016](ADR-016-request-dsl.md). The DSL grammar requires no change
(see Context §"Phantom gaps").

---

## Context

### The disease: documentation drift against a compiled surface

khive's ontology -- 9 entity kinds, 17 edge relations, 5 base note kinds (9 under the
default pack set today), and the per-relation endpoint matrix -- is a closed, compiled
contract. The base endpoint allowlist lives in
`BASE_ENTITY_ENDPOINT_RULES` (`crates/khive-runtime/src/operations.rs`), extended additively
by each loaded pack's `EDGE_RULES` (ADR-017). No runtime surface exposes this merged
contract. Agents learn it from documentation snapshots that provably rot:

- **Measured casualty**: a fleet skill in production taught 11 relations and 4 entity
  kinds against the real 17 and 9. Agents consuming that skill gave wrong answers to
  endpoint-legality questions until the drift was caught by hand.
- **Endpoint-legality questions** ("can a `resource` be the source of an `annotates`
  edge?") currently require reading Rust source or ADRs. A recorded wrong-answer incident
  established the standing instruction "verify edge endpoints against `operations.rs`
  RULES, not ADRs" -- an instruction that concedes the documentation cannot be trusted.

### Phantom capability gaps: the strongest evidence

Three friction items on the motivating list turned out, under verification, to be
capabilities that already ship. The fleet believed the surface could not do things it does:

1. **Multi-line strings in the function-call DSL.** Believed impossible; escapes were
   specified in ADR-016 from the start ("String escapes follow JSON: `\\`, `\"`, `\n`,
   `\t`, `\r`") and the lexer implements them. Only raw control-character bytes fail,
   with an error message that does not point at the escape syntax. Multiple independent
   agent memories recorded "must switch to the JSON op form" as a workaround for a
   problem that does not exist at the DSL layer. The friction is nonetheless real one
   layer up: the `ops` string travels as a JSON value in the MCP tool call, so a caller
   who writes `\n` in that JSON has it decoded to a literal newline byte _by the
   transport_ before the DSL parser sees it. The working form from an MCP client is the
   double escape `\\n`, which nobody discovers unaided from the current error. The gap
   is discoverability, not grammar -- addressed normatively in §3c.
2. **`memory.remember` tags.** Believed missing; `RememberParams` carries
   `tags: Option<Vec<String>>` and the handler persists them.
3. **`recall | auto_feedback` chaining.** Believed impossible ("`$prev` cannot address a
   bare array result"); `$prev[N].field` addressing is specified in ADR-016 and works,
   including nested inside an array-of-objects argument literal. Verified live:
   `memory.recall(...) | brain.auto_feedback(results=[{"id": $prev[0].id}])` succeeds.

The lesson is uniform: when the surface cannot describe itself, agents substitute folklore
for the contract, and folklore decays into false limitations. Prose discipline has not
fixed this and will not.

### Live asymmetries across the verb surface

Independent of drift, the verb surface has accumulated real inconsistencies as packs were
added, each individually small, collectively a memorization tax:

- **Param-name divergence**: `query` vs `q`, `at` (not `due`), `id` (not `thread_id`),
  `ids` (not `slugs`) -- correct names are fleet folklore, not a stated convention.
- **ID-resolution divergence**: most `id`-typed params accept a short unique prefix;
  `brain.feedback` requires a full UUID; `knowledge.get` accepts slug or full UUID but
  not a prefix.
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

## Decision

One contract, one introspection verb. The contract states the invariants every pack's
verb surface must obey; the `schema` verb is the contract's runtime projection -- the
machine-readable source of truth that makes the ontology legible to agents and makes
documentation generatable instead of hand-maintained.

### 1. The consistency contract

Normative rules binding all packs compiled into the kkernel binary. Each rule names its
current violators; violators become tracked conformance failures (issues), not
grandfathered exceptions.

**Rule 1 -- ID-resolution ladder.** Every parameter that accepts a record identifier
resolves, in order: full UUID → registered slug (where the verb declares slug support) →
short unique hex prefix. A prefix that matches zero or multiple records is an error naming
the ambiguity. Violators at ratification: `brain.feedback` (full UUID only),
`knowledge.get` (slug or full UUID, no prefix).

**Rule 2 -- no silent enum coercion.** A parameter with a closed value set rejects
unrecognized values with an error listing the valid values. Defaulting applies only to
_absent_ parameters, never to _invalid_ ones. Violator at ratification: `parse_direction`
(invalid `direction` coerces to `Out`).

**Rule 3 -- help-schema fidelity.** The `help=true` envelope for a verb is generated from
the same `ParamDef` slice the registry serves; a documented default must be the default
the code applies. (The `neighbors` direction default -- docs said `both`, code applies
`Out` -- is the motivating instance.)

**Rule 4 -- param-naming conventions.** Canonical names for recurring concepts: `query`
(never `q`), `id` (never `thread_id` or verb-specific synonyms for the primary record
identifier), `ids` (never `slugs` for plural identifiers), `at` (never `due` for
schedule datetimes). This rule binds **new verbs at review time**. Renaming a shipped
parameter is a breaking change to a public surface; existing non-conforming params are
migrated only through an explicit deprecation path -- accept both names for a stated
window, warn on the deprecated one, remove in a versioned release. No silent renames.
Whether any existing param is worth that migration cost is decided case by case at
sign-off, not by this contract.

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
ratification: comm declares `message` and schedule declares `scheduled_event` in their
`Pack` impls -- the default pack set has no violator. Enforcement: review-level for new
packs, plus the end-to-end smoke test asserting that every kind present in a driven store
run appears in the merged declared vocabulary (`HandlerDef` carries no write-set, so a
static phase-1 check is impossible; this is a §4 Phase-2-class boundary, stated not
hidden).

### 2. The `schema` verb

A new kg-pack verb (bare name, per ADR-023 naming rules), assertive, read-only,
no side effects. It returns the merged live ontology of the running binary:

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
    "task",
    "memory",
    "message",
    "scheduled_event"
  ],
  "edge_relations": [
    "contains",
    "part_of",
    "instance_of",
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
    "refutes"
  ],
  "endpoint_rules": [
    { "source": "concept", "relation": "extends", "target": "concept", "origin": "base" },
    { "source": "*", "relation": "instance_of", "target": "concept", "origin": "base" },
    { "source": "task", "relation": "depends_on", "target": "task", "origin": "pack:gtd" }
  ],
  "packs_loaded": ["kg", "gtd", "memory", "brain", "comm", "schedule", "knowledge"],
  "contract_version": "<schema content hash>"
}
```

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
   already exposed via `base_entity_endpoint_rules()` (added for the ADR-076 certificate
   tests). Pack rules, however, are currently only reachable through helpers that
   flatten ownership away (`all_edge_rules` / the registry's installed
   `Vec<EdgeEndpointRule>`), which cannot reconstruct `origin`. The implementation MUST
   therefore add a pack-attributed accessor returning `(pack_name, EdgeEndpointRule)`
   rows (the registry knows the owning pack at installation time) and serialize from
   that; the existing flattened helpers are not a sufficient source for this verb.

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

**3c. DSL string-error discoverability.** The parse error for a raw control character
inside a quoted string must teach the fix: state that string escapes follow JSON, show
the escape form (`\n`), and note that callers passing `ops` through a JSON transport
(every MCP client) must double-escape (`\\n` in the wire form) because the transport
decodes one level before the DSL parser runs. The canonical multi-line guidance --
escape form and the transport double-escape -- is documented in the generated `AGENTS.md`
DSL section (§5). The grammar itself is unchanged (see Alternatives §6).

**3d. Tracked conformance failures (Rules 1-3).** `brain.feedback` full-UUID-only,
`knowledge.get` missing prefix resolution, `parse_direction` silent coercion, and the
`neighbors` help/default mismatch are recorded as conformance issues at ratification.
Their fixes are ordinary bugfix PRs referencing this ADR; none of them is gated on the
`schema` verb landing.

### 4. The conformance test -- two declared phases

A workspace test walks every registered pack's `HandlerDef` slices and asserts the
mechanically checkable subset of the contract.

**Phase 1 (ships with this ADR's implementation).** Checkable from today's `ParamDef`
(`{name, param_type, required, description}`, where `param_type` is a free-form
documentation string not used in validation):

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

**Phase 2 (declared, not shipped here).** Rules 1 and 2 are _behavioral_ -- prefix
resolution and enum rejection cannot be verified from a free-form type string. Enforcing
them mechanically requires enriching `ParamDef` with a typed `kind` (e.g.
`ParamKind::{Uuid, Enum(&[...]), String, Int, Bool, Array}`) and an ID-resolution flag,
plumbed through the pack/registry/catalog layers. That enrichment is its own change with
its own review; until it lands, Rules 1-2 are enforced by review and by the tracked-issue
list, and this ADR does not claim otherwise.

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

- One more kg verb (16 → 17) to document and maintain; mitigated by it being a
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
   already specified, so no grammar addition is needed. A variant was also considered --
   tolerating raw control characters inside quoted strings, which would make the naive
   single-escaped form work with zero failed round-trips. Rejected because it breaks the
   "string escapes follow JSON" contract, and an unterminated quote would then swallow
   subsequent lines and surface as a confusing failure far from the fault. The
   discoverability failure that motivated both variants is real and is addressed
   normatively by §3c (a teaching error message covering the transport double-escape)
   plus generated documentation (§5); a naive caller pays at most one self-explaining
   failed call instead of a folklore hunt.

## References

- [ADR-016](ADR-016-request-dsl.md) -- request DSL; string escapes and `$prev[N].field`
  addressing (both verified shipped, unamended)
- [ADR-017](ADR-017-pack-standard.md) -- Pack trait, `EDGE_RULES`, pack-extensible
  endpoints (the merge semantics `schema` serializes)
- [ADR-023](ADR-023-declarative-pack-format.md) -- verb surface and naming rules
  (amended: `schema` row in the kg verb table)
- [ADR-045](ADR-045-verb-response-presentation.md) -- presentation rules applied to `schema`
  output
- [ADR-048](ADR-048-knowledge-section-profiles.md) -- `resource` kind (pack-declared;
  why §2 serializes merged vocabulary)
- [ADR-076](ADR-076-relation-calculability-and-system-role.md) -- certificate tests that motivated
  `base_entity_endpoint_rules()`
- `crates/khive-runtime/src/operations.rs` -- `BASE_ENTITY_ENDPOINT_RULES`
- `crates/khive-types/src/pack.rs` -- `ParamDef` / `HandlerDef` (phase-1 test substrate)
