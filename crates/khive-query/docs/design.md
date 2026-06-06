# khive-query Design

## ADR Compliance

### ADR-008: Query Layer Separation

This crate implements the query parsing and compilation pipeline described in ADR-008.
It is intentionally split into three stages:

1. **Parse** (`parsers/gql.rs`, `parsers/sparql.rs`) — hand-written recursive descent
   parsers that convert GQL or SPARQL text into a shared `GqlQuery` AST.
2. **Validate** (`validate.rs`) — normalizes edge relation strings to canonical
   snake_case, rejects `namespace` in query text (scoping is `CompileOptions::scopes`
   only), and enforces the 10-hop traversal depth cap.
3. **Compile** (`compilers/sql.rs`) — lowers the validated AST to parameterized SQL
   for execution by the runtime.

**Key design decisions:**
- `QueryValue` deliberately mirrors only the subset of `khive_storage::types::SqlValue`
  that the query compiler needs to emit. The runtime converts these to storage-layer
  `SqlValue` at the query–storage boundary. This keeps the query crate dependent only
  on `khive-types`, not on `khive-storage` or `khive-db`.
- `WhereExpr` supports AND, OR, and leaf conditions. The tree is compiled preserving
  SQL OR/AND connectives rather than flattening to AND-only.
- GQL WHERE grammar: `where_expr = and_expr ('OR' and_expr)*` where `and_expr =
  condition ('AND' condition)*`. AND binds tighter than OR.
- Node kind strings are pack-agnostic and pass through the query layer unchanged.
  Kind validation is a pack-handler concern, not a query-layer concern.
- `namespace` is always injected via `CompileOptions.scopes`, never from query text.
  Any attempt to set `namespace` in a query node property or WHERE condition is
  rejected at validation time.

### ADR-041: Synthetic Observation Edge Paths

Relations prefixed `observed_as_*` (specifically: `observed_as_candidate`,
`observed_as_selected`, `observed_as_target`, `observed_as_signal`) are synthetic
edges that join against `event_observations`, not `graph_edges`.

**Key design decisions:**
- Only the four known `observed_as_*` strings are valid. Unknown `observed_as_bogus`
  strings are rejected at validation with the closed list of valid values. This closes
  the bypass that would allow arbitrary strings to compile as `graph_edges` queries.
- Synthetic edges are always outbound (event → entity/note). Inbound or undirected
  synthetic edges are rejected at compile time.
- Synthetic edges cannot be variable-length (used in a `*N..M` range). The recursive
  CTE targets `graph_edges` only; `event_observations` has no recursive path structure.
- Mixed synthetic + canonical relations in a single edge pattern are rejected. The two
  tables do not share a join key that would make OR across them meaningful.
- Event source nodes bind to the `events` table; observation target nodes bind to the
  `notes` table (discriminated by `referent_kind = 'note'` in `event_observations`).
- Event nodes do not have `entity_type` or arbitrary `properties` — these are rejected
  at compile time with an actionable error.

## Security Invariants

- **Namespace injection** (MAJ-1): `namespace` always comes from `CompileOptions.scopes`,
  never from query text. Bound as a parameter, never as a SQL literal.
- **Edge property whitelist** (MAJ-2): only `relation` and `weight` are queryable edge
  columns. Any other property name (e.g. `source_id`) is rejected with the valid list.
- **Depth cap** (MAJ-3): traversal depth is capped at `MAX_DEPTH` (10 hops). Exceeding
  it is an `InvalidInput` error, not a silent clamp. The cap is always a bound parameter
  in recursive CTEs, never a SQL literal.
- **OR spanning endpoints** in variable-length patterns: a `WHERE` clause that references
  both start and end endpoint variables across an OR node is rejected. Single-endpoint
  ORs (e.g. `a.name='X' OR a.name='Y'`) are correctly compiled.

## Compilation Paths

### Fixed-length (all edges `*1..1`)

Compiles to a JOIN chain:

```
MATCH (a:concept)-[e:introduced_by]->(b:paper)
→
SELECT … FROM entities a
JOIN graph_edges e ON e.source_id = a.id
JOIN entities b ON b.id = e.target_id
WHERE … LIMIT ?
```

### Variable-length (any edge `*N..M` where M > 1)

Compiles to a recursive CTE. Only a single `start_node -[*N..M]-> end_node` pattern
is supported; mixed fixed+variable chains are rejected. The recursive member joins
`entities next_node` to filter soft-deleted intermediate nodes and enforce namespace
scoping on traversal paths.

```
WITH RECURSIVE traverse(…) AS (
    SELECT … FROM entities s JOIN graph_edges e … WHERE …   -- seed
    UNION ALL
    SELECT … FROM traverse t JOIN graph_edges e … JOIN entities next_node …
    WHERE t.depth < ?max_depth AND … NOT LIKE …  -- anti-cycle
)
SELECT DISTINCT … FROM traverse t JOIN entities r … WHERE … LIMIT ?
```

## Consistency Notes

- `SPARQL '*'` (zero-or-more hops) is not supported. The recursive CTE seed starts at
  depth 1 and cannot emit a depth-0 row. Rejecting it prevents silently treating `*` as
  `+` and dropping valid zero-hop matches.
- Repeated node variables (cycle / self-reachability patterns like
  `(a)-[:extends]->(b)-[:variant_of]->(a)`) are rejected at validation. Supporting
  them requires alias-equality predicates in the SQL that are not yet implemented.
- `validate_pattern_shape` is called both from `validate_with_warnings` (parser output)
  and from `compile` (public API boundary) to catch hand-constructed malformed ASTs that
  skip the parser.
- The `parse_auto` fallback for unrecognized prefixes uses the GQL parser to preserve
  existing behavior for unknown prefixes.
