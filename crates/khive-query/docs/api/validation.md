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

## Absence block scope

Each `WhereExpr::NotExists` is validated independently against the outer node/edge binding map. It must contain one nonempty alternating path and at least one node name from the outer MATCH. A correlated singleton is allowed. Repeated inner nodes or edges and node/edge name conflicts remain refused, including conflicts with outer binding kinds. An inner edge reusing an outer edge name is refused with that variable named and the explanation that edge variables are not correlated into NOT EXISTS; it cannot silently shadow the outer edge.

Additional variables are local to that block. Separate blocks can reuse local names without linking their scopes. The outer scalar-condition walkers deliberately skip these patterns, so local names do not become valid outer WHERE, RETURN, count or ORDER references. Validation/lowering retain the existing unknown-binding refusals.

Every inner edge requires `min_hops == max_hops == 1`, and every relation is normalized through the canonical taxonomy. Synthetic relations, including mixed alternatives, and zero/recursive ranges refuse. Namespace properties are forbidden on every inner node. Each correlated occurrence retains its own kind, subtype and inline-property constraints; contradictory constraints yield no inner match. These checks apply to direct ASTs as well as parsed GQL.

## Relation taxonomy

Canonical relations are parsed through the closed `EdgeRelation` taxonomy and normalized to canonical snake case. Punctuation that is not part of a legal relation is rejected, not normalized away.

Four ADR-041 projections are accepted outside the canonical graph-edge enum:

- `observed_as_candidate`
- `observed_as_selected`
- `observed_as_target`
- `observed_as_signal`

Any other `observed_as_*` spelling is rejected, closing the route by which an arbitrary synthetic-looking relation could otherwise reach canonical edge compilation. With multiple comma paths, every edge must be canonical: even known synthetic relations return `QueryError::Unsupported`. Existing single-path synthetic behavior is unchanged.

## Hop bounds

`MAX_DEPTH` is 10. A zero minimum is accepted, including `0..0` and `0..1`; these ranges use traversal compilation rather than a fixed one-hop JOIN. Validation still rejects inverted ranges, a minimum above the cap, or any maximum above the cap. Bounds are never silently swapped or clamped because that changes query meaning. For multiple paths, each edge must additionally have `min_hops == max_hops == 1`; recursive multi-pattern matches return `Unsupported`. A path may still contain several fixed one-hop edges.

For a single canonical path, the sum of all variable-segment maxima must also be at most ten. The sum uses checked arithmetic after the existing per-edge validation; exceeding it returns `InvalidInput`. Fixed `1..1` steps do not count toward the sum, including parsed `*1` and `*1..1`; `0..0` contributes zero. A path with any variable segment cannot include synthetic edges, even as a fixed prefix or suffix.

The shared bounds apply to GQL ranges and explicit SPARQL `{min,max}` ranges, including composed paths. Bare SPARQL `*` remains unsupported; bare GQL `*` retains its existing one-through-five-hop meaning. Variable-length synthetic edges and recursive comma patterns remain unsupported.

## Namespace, kinds, and conditions

Namespace scope is runtime input through `CompileOptions.scopes`; `namespace` in a node property map or `WHERE` condition is rejected. This prevents query text from overriding caller-supplied scope.

Node kinds are pack-agnostic strings and are neither validated nor renamed in this crate. In particular, historical aliases such as `paper` do not become `document` here.

Mixed canonical paths evaluate the complete WHERE Boolean tree on the joined binding row before grouping or paging; OR may span fixed-step and variable-segment bindings. Existing single-variable-segment seed/final phase OR refusal is unchanged.

Condition validation sees bindings from every path. Shared-node references resolve through the first occurrence during lowering, while all occurrence constraints survive. The global WHERE expression tree is retained. Condition validation is binding-aware. A direct node or observation-target field must name an allowed dedicated column; arbitrary and nested JSON data requires an explicit `properties.<path>` reference. The bare `properties` container, unknown direct fields, and JSON paths on bindings without a properties object are rejected. Event predicates accept only event columns, while edge predicates accept `relation` and `weight`; rejected names report the valid list. `relation` values on edge variables are taxonomy-checked. Operators that require a particular value shape, including list-valued `IN` and operand-free `IS NOT NULL`, are checked before compilation.
