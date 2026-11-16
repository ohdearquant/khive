# AST Validation and Normalization

Validation enforces the invariants required by SQL compilation and canonicalizes edge relations in place. It does not validate pack-owned node kind strings, which remain unchanged for the loaded pack to interpret.

## `validate` and `validate_with_warnings`

`validate(query)` applies all checks and discards the warning vector. `validate_with_warnings(query)` returns the same result plus warnings; the current implementation emits an empty vector but preserves the surface for future non-fatal diagnostics.

Both functions mutate canonical relation spellings in place. They return `QueryError::Validation` for structural or ontology violations and `QueryError::InvalidInput` when a hop bound exceeds the hard depth limit.

## Pattern shape and bindings

`validate_pattern_shape(elements)` accepts an empty slice so the compiler can report its more specific empty-pattern error. A non-empty pattern must have odd length and alternate node/edge/node from index zero.

Each path retains the repeated-node refusal. Across paths, a named node denotes the same `(substrate kind, UUID)`, and all occurrences' kind, subtype and inline-property constraints apply conjunctively. Conflicting constraints produce no rows. Variable names are case-sensitive. Edge names must be unique across the whole query; a name cannot bind both a node and an edge. Different edge names may select the same record.

Multiple paths must form one connected set through shared named nodes. Connectivity is checked over the complete set, allowing a later path to bridge earlier paths. Anonymous occurrences cannot connect components. A shared singleton node path is supported as an additional constraint; disconnected components return `QueryError::Unsupported`. Every path receives the alternating-shape check, and empty additional paths return `Validation`. These checks also apply to directly constructed ASTs.

Cross-path cycles are permitted within this fixed canonical shape. The existing repeated-node diagnostic within a single path remains unchanged; this does not introduce general recursive cycle matching.

## Relation taxonomy

Canonical relations are parsed through the closed `EdgeRelation` taxonomy and normalized to canonical snake case. Punctuation that is not part of a legal relation is rejected, not normalized away.

Four ADR-041 projections are accepted outside the canonical graph-edge enum:

- `observed_as_candidate`
- `observed_as_selected`
- `observed_as_target`
- `observed_as_signal`

Any other `observed_as_*` spelling is rejected, closing the route by which an arbitrary synthetic-looking relation could otherwise reach canonical edge compilation. With multiple comma paths, every edge must be canonical: even known synthetic relations return `QueryError::Unsupported`. Existing single-path synthetic behavior is unchanged.

## Hop bounds

`MAX_DEPTH` is 10. Validation rejects zero minimum hops because neither fixed JOIN compilation nor the recursive CTE can emit depth-zero results. It also rejects inverted ranges, a minimum above the cap, or any maximum above the cap. Bounds are never silently swapped or clamped because that changes query meaning. For multiple paths, each edge must additionally have `min_hops == max_hops == 1`; recursive multi-pattern matches return `Unsupported`. A path may still contain several fixed one-hop edges.

## Namespace, kinds, and conditions

Namespace scope is runtime input through `CompileOptions.scopes`; `namespace` in a node property map or `WHERE` condition is rejected. This prevents query text from overriding caller-supplied scope.

Node kinds are pack-agnostic strings and are neither validated nor renamed in this crate. In particular, historical aliases such as `paper` do not become `document` here.

Condition validation sees bindings from every path. Shared-node references resolve through the first occurrence during lowering, while all occurrence constraints survive. The global WHERE expression tree is retained. Condition validation is binding-aware. A direct node or observation-target field must name an allowed dedicated column; arbitrary and nested JSON data requires an explicit `properties.<path>` reference. The bare `properties` container, unknown direct fields, and JSON paths on bindings without a properties object are rejected. Event predicates accept only event columns, while edge predicates accept `relation` and `weight`; rejected names report the valid list. `relation` values on edge variables are taxonomy-checked. Operators that require a particular value shape, including list-valued `IN` and operand-free `IS NOT NULL`, are checked before compilation.
