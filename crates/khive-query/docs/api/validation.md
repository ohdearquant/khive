# AST Validation and Normalization

Validation enforces the invariants required by SQL compilation and canonicalizes edge relations in place. It does not validate pack-owned node kind strings, which remain unchanged for the loaded pack to interpret.

## `validate` and `validate_with_warnings`

`validate(query)` applies all checks and discards the warning vector. `validate_with_warnings(query)` returns the same result plus warnings; the current implementation emits an empty vector but preserves the surface for future non-fatal diagnostics.

Both functions mutate canonical relation spellings in place. They return `QueryError::Validation` for structural or ontology violations and `QueryError::InvalidInput` when a hop bound exceeds the hard depth limit.

## Pattern shape and bindings

`validate_pattern_shape(elements)` accepts an empty slice so the compiler can report its more specific empty-pattern error. A non-empty pattern must have odd length and alternate node/edge/node from index zero.

Node and edge variables are bindings. Repeating a binding would require SQL alias-equality predicates to express cycles or self-reachability; until that support exists, repeated names are rejected instead of compiling to incorrect independent aliases.

## Relation taxonomy

Canonical relations are parsed through the closed `EdgeRelation` taxonomy and normalized to canonical snake case. Punctuation that is not part of a legal relation is rejected, not normalized away.

Four ADR-041 projections are accepted outside the canonical graph-edge enum:

- `observed_as_candidate`
- `observed_as_selected`
- `observed_as_target`
- `observed_as_signal`

Any other `observed_as_*` spelling is rejected, closing the route by which an arbitrary synthetic-looking relation could otherwise reach canonical edge compilation.

## Hop bounds

`MAX_DEPTH` is 10. Validation rejects zero minimum hops because neither fixed JOIN compilation nor the recursive CTE can emit depth-zero results. It also rejects inverted ranges, a minimum above the cap, or any maximum above the cap. Bounds are never silently swapped or clamped because that changes query meaning.

## Namespace, kinds, and conditions

Namespace scope is runtime input through `CompileOptions.scopes`; `namespace` in a node property map or `WHERE` condition is rejected. This prevents query text from overriding caller-supplied scope.

Node kinds are pack-agnostic strings and are neither validated nor renamed in this crate. In particular, historical aliases such as `paper` do not become `document` here.

Condition validation is binding-aware. `relation` is taxonomy-checked only on an edge variable, while `kind` has node semantics only on a node variable; the same names on the opposite substrate remain ordinary JSON property keys. Operators that require a particular value shape, including list-valued `IN` and operand-free `IS NOT NULL`, are checked before compilation.
