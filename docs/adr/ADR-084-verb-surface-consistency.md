# ADR-084: Verb-Surface Consistency Contract and Live Ontology Introspection

**Status**: Accepted
**Date**: 2026-07-02
**Depends on**: [ADR-016](./ADR-016-request-dsl.md),
[ADR-017](./ADR-017-pack-standard.md),
[ADR-023](./ADR-023-declarative-pack-format.md),
[ADR-045](./ADR-045-verb-response-presentation.md),
[ADR-076](./ADR-076-relation-calculability-and-system-role.md)

## Context

Verb declarations, parser behavior, help output, and documentation are separate
representations of one public surface. If defaults, identifier handling, enum validation,
or taxonomy lists drift, callers receive contradictory contracts.

The running registry already knows which packs, verbs, kinds, relations, and endpoint rules
are loaded. That compiled state should be inspectable and should generate documentation.

## Decision

Adopt one consistency contract for every registered verb and add a read-only `schema`
verb that projects the live ontology of the running registry.

### 1. Identifier resolution

Every parameter declared as a record identifier uses the same ordered resolution policy:

1. full UUID;
2. registered slug where that parameter explicitly supports slugs; and
3. unique short hexadecimal prefix.

Zero or multiple prefix matches are typed errors. A verb that intentionally accepts only
full UUIDs must declare that narrower type in its parameter schema rather than silently
using a different resolver.

### 2. Closed values reject invalid input

A closed enum accepts a default only when the parameter is absent. An unrecognized present
value returns an error listing valid values. Silent coercion to a default or nearest value
is prohibited.

### 3. Help fidelity

`help=true` is generated from the same `ParamDef` data used by the registry.
Descriptions, required flags, defaults, enum values, and identifier semantics must match
runtime behavior.

### 4. Parameter naming

New verbs use common names consistently:

| Concept                   | Canonical name |
| ------------------------- | -------------- |
| Search text               | `query`        |
| Primary record identifier | `id`           |
| Multiple identifiers      | `ids`          |
| Time instant              | `at`           |
| Namespace attribution     | `namespace`    |
| Tags                      | `tags`         |

A shipped non-conforming name changes only through an explicit versioned compatibility
plan. Silent renames are prohibited.

### 5. Substrate symmetry

Fields representing the same concept use the same public name across substrates. Entity and
note tags are both exposed as `tags`, even if their storage layouts differ.

Fields representing different concepts remain distinct. Entity `description` and note
`content` do not become aliases. An error for the wrong field names the correct field for
the selected substrate.

### 6. Declared vocabulary completeness

A pack declares every entity kind and note kind it writes unless a required pack owns that
declaration. Every contributed endpoint rule retains its origin.

Writing an undeclared kind is a conformance failure because live schema output would
otherwise under-describe stored data.

### 7. The `schema` verb

`schema` is an assertive, read-only kg verb with no parameters in v1. It returns the
merged live ontology:

```json
{
  "entity_kinds": ["concept", "document", "dataset"],
  "note_kinds": ["observation", "insight", "question"],
  "edge_relations": ["contains", "part_of", "annotates"],
  "endpoint_rules": [
    {
      "source": "concept",
      "relation": "extends",
      "target": "concept",
      "origin": "base"
    }
  ],
  "packs_loaded": ["kg"],
  "contract_version": "<schema-content-hash>"
}
```

The example is illustrative. Runtime output is assembled from loaded declarations and is
the authority.

Normative behavior:

- kinds are the sorted union of loaded pack declarations;
- relations are the complete closed runtime enum;
- endpoint rows preserve `origin = "base"` or `origin = "pack:<name>"`;
- wildcard endpoints are emitted as `"*"`, not expanded;
- duplicates are removed only when the full semantic row is identical;
- conflicting declarations are startup errors;
- `contract_version` is a deterministic hash of canonical output excluding the hash
  field itself; and
- presentation follows ADR-045.

`verbs` and `help=true` describe callable operations. `schema` describes the data
model. Neither replaces the other.

### 8. Conformance checks

Registry tests mechanically assert:

- no duplicate parameter names within a verb;
- every verb and parameter has a non-empty description;
- new parameter names follow the convention table;
- defaults and required flags do not conflict;
- enum declarations contain no duplicates;
- each declared identifier type selects an allowed resolver; and
- every endpoint rule references declared kinds and relations.

Behavioral tests drive representative handlers to assert:

- identifier resolution matches the declaration;
- invalid enums reject rather than coerce;
- help defaults match execution;
- every kind observed in a driven store run appears in merged vocabulary; and
- schema output is deterministic across registration order.

External packs are not compiled into repository tests, but startup validation and live
introspection make their deviations visible.

### 9. Generated documentation

Taxonomy and endpoint-matrix documentation is generated from canonical `schema` output.
A build check compares committed generated sections with output from the built registry.
Hand-written explanatory prose may surround those sections but cannot restate a competing
enumeration.

### 10. DSL string errors

When a quoted DSL string contains an unescaped control character, the parser error explains
that DSL escapes follow JSON and shows the escaped form. When input arrives inside a JSON
transport, the message also notes that one additional escaping layer is required.

## Invariants

- Registry behavior, help, and schema share declarations.
- Invalid present values never become defaults.
- The live vocabulary includes every writable kind.
- Rule origin is not lost during registry flattening.
- Schema hashing is deterministic.
- Generated taxonomy sections cannot drift silently.

## Verification

Tests must cover:

- full, slug-enabled, and unique-prefix identifier paths;
- ambiguous and missing prefix errors;
- enum absence versus invalid presence;
- help/runtime default equivalence;
- note/entity tag symmetry;
- wrong-substrate field guidance;
- declared-vocabulary completeness;
- conflicting endpoint declarations;
- stable schema ordering and hash;
- table and agent presentation; and
- generated-documentation freshness.

## Alternatives considered

| Alternative                                          | Reason rejected                                                       |
| ---------------------------------------------------- | --------------------------------------------------------------------- |
| Maintain taxonomy prose manually                     | Repeats compiled facts and permits drift.                             |
| Serialize only base enums                            | Omits loaded pack declarations and rule origins.                      |
| Keep endpoint ownership only in flattened rows       | Cannot explain which pack extended the ontology.                      |
| Make schema filterable in v1                         | The full matrix is bounded; filters add surface before measured need. |
| Treat legacy inconsistencies as permanent exceptions | Makes one public contract impossible to state.                        |

## Consequences

### Positive

- Callers can inspect the exact runtime ontology.
- Help, validation, and documentation converge on shared declarations.
- Pack vocabulary and endpoint origins remain visible.

### Negative

- Packs must declare all writable kinds and accurate parameter metadata.
- Behavioral conformance still requires driven tests; declarations alone cannot prove it.
- Shipped naming inconsistencies require explicit compatibility work if changed.

## References

- [ADR-016](./ADR-016-request-dsl.md): request grammar
- [ADR-017](./ADR-017-pack-standard.md): declarations and endpoint rules
- [ADR-023](./ADR-023-declarative-pack-format.md): registry composition
- [ADR-045](./ADR-045-verb-response-presentation.md): presentation modes
- [ADR-076](./ADR-076-relation-calculability-and-system-role.md): ontology certificate
