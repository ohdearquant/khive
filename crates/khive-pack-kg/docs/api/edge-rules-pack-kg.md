# `KG_EDGE_RULES` (pack.rs)

Technical reference for the kg pack's own additive edge-endpoint extensions, layered over
the ADR-002 base allowlist per ADR-017's pack-extensible endpoint rules.

Adds person→org, person→project, and org→org pairs to the base edge-endpoint allowlist. The
person→project rows mirror person→org (issue #60): a person is a member of a project the
same way they are a member of an org, so the same member-not-component semantic stretch
accepted for person→org is extended here.

## Test rationale (`pack.rs::tests`)

- `kg_pack_edge_rules_contain_no_duplicate_triples`: a duplicated `(relation, source,
  target)` triple would be a no-op additive rule (adding the same endpoint pair a second time
  changes nothing) and is a sign of a copy-paste error. Semantic similarity between relations
  (e.g. multiple relations accepting `org→org`) is expected and correct; the test checks only
  for exact-triple duplicates, not for shared per-relation endpoint sets.
- `kg_pack_edge_rules_cover_expected_relations`: a deliberate-change tripwire over the live
  `KG_EDGE_RULES`, complementing the ADR-076 §D2 non-redundancy certificate in the
  certificate test suite. A change to the set of relations that get pack-level endpoint
  extensions should be a deliberate, reviewed decision — not an accidental side effect.
