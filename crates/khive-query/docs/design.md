# khive-query Design

Backend-agnostic GQL/SPARQL parsing and SQL compilation crate. Parses query text
into a shared `GqlQuery` AST, validates edge relations against the closed
`EdgeRelation` taxonomy, and compiles the AST to parameterized SQL for execution
by the runtime. The crate depends only on `khive-types` (for `EdgeRelation`); it
has no dependency on storage, DB, or runtime crates.

## ADR Links

- [ADR-001: Entity Kind Taxonomy](../../../docs/adr/ADR-001-entity-kind-taxonomy.md)
- [ADR-002: Edge Ontology](../../../docs/adr/ADR-002-edge-ontology.md)
- [ADR-008: Query Layer Separation](../../../docs/adr/ADR-008-query-layer-separation.md)
- [ADR-041: Event Provenance Projection](../../../docs/adr/ADR-041-event-provenance-projection.md)

## Modules

- [`src/ast.rs`](../src/ast.rs) -- GQL abstract syntax tree types
- [`src/parsers/gql.rs`](../src/parsers/gql.rs) -- hand-written recursive descent
  GQL parser
- [`src/parsers/sparql.rs`](../src/parsers/sparql.rs) -- SPARQL-inspired syntax
  parser
- [`src/validate.rs`](../src/validate.rs) -- AST validation and relation
  normalization
- [`src/compilers/sql.rs`](../src/compilers/sql.rs) -- SQL compiler (fixed-length
  JOIN chain + variable-length recursive CTE)
- [`src/error.rs`](../src/error.rs) -- query-layer error types

## Tests

- Inline unit tests in `compilers/sql.rs`, `parsers/gql.rs`, `parsers/sparql.rs`
  (access private helpers and internal types)
- Extracted tests in `src/validate_tests.rs` (via `#[path]` attribute from
  `validate.rs`)

## Benchmarks

- [`benches/parse_bench.rs`](../benches/parse_bench.rs) -- Criterion parse-latency
  benchmarks
- See [benchmarks.md](benchmarks.md) for the ledger

## ADR Compliance

### ADR-008: Query Layer Separation

This crate implements the query parsing and compilation pipeline described in
ADR-008. It is intentionally split into three stages:

1. **Parse** (`parsers/gql.rs`, `parsers/sparql.rs`) -- hand-written recursive
   descent parsers that convert GQL or SPARQL text into a shared `GqlQuery` AST.
2. **Validate** (`validate.rs`) -- normalizes edge relation strings to canonical
   snake_case, rejects `namespace` in query text (scoping is
   `CompileOptions::scopes` only), and enforces the 10-hop traversal depth cap.
3. **Compile** (`compilers/sql.rs`) -- lowers the validated AST to parameterized
   SQL for execution by the runtime.

**Key design decisions:**

- `QueryValue` deliberately mirrors only the subset of
  `khive_storage::types::SqlValue` that the query compiler needs to emit. The
  runtime converts these to storage-layer `SqlValue` at the query-storage
  boundary. This keeps the query crate dependent only on `khive-types`, not on
  `khive-storage` or `khive-db`.
- `WhereExpr` supports AND, OR, and leaf conditions. The tree is compiled
  preserving SQL OR/AND connectives rather than flattening to AND-only.
- GQL WHERE grammar: `where_expr = and_expr ('OR' and_expr)*` where
  `and_expr = condition ('AND' condition)*`. AND binds tighter than OR.
- GQL WHERE conditions support `=`, `!=`, `>`, `<`, `>=`, `<=`, `LIKE`,
  `CONTAINS`, `STARTS WITH`, `IN` with a list literal, and `IS NOT NULL`.
  `CONTAINS` and `STARTS WITH` treat `%`, `_`, and `\` as literal characters.
  All condition values are emitted as bound SQL parameters.
- Node kind strings are pack-agnostic and pass through the query layer unchanged.
  Kind validation is a pack-handler concern, not a query-layer concern.
- `namespace` is always injected via `CompileOptions.scopes`, never from query
  text. Any attempt to set `namespace` in a query node property or WHERE condition
  is rejected at validation time.

### ADR-041: Synthetic Observation Edge Paths

Relations prefixed `observed_as_*` (specifically: `observed_as_candidate`,
`observed_as_selected`, `observed_as_target`, `observed_as_signal`) are synthetic
edges that join against `event_observations`, not `graph_edges`.

**Key design decisions:**

- Only the four known `observed_as_*` strings are valid. Unknown
  `observed_as_bogus` strings are rejected at validation with the closed list of
  valid values.
- Synthetic edges are always outbound (event -> entity/note). Inbound or
  undirected synthetic edges are rejected at compile time.
- Synthetic edges cannot be variable-length. The recursive CTE targets
  `graph_edges` only.
- Mixed synthetic + canonical relations in a single edge pattern are rejected.
- Event source nodes bind to the `events` table; observation target nodes bind to
  the `notes` table (discriminated by `referent_kind = 'note'`).
- Event nodes do not have `entity_type` or arbitrary `properties` -- these are
  rejected at compile time with an actionable error.

### Inline Property-Map Literal Grammar (issues #755, #832)

Node patterns support an inline property map: `(n:kind {key: value, ...})`. The
grammar for `value` (shared with WHERE-clause condition values, via the same
`parse_value` parser) is:

```text
value      = string | integer | float | bool
string     = "'" ... "'" | '"' ... '"'
integer    = ["-"] digit+                     -- no "." in the lexeme
float      = ["-"] digit+ "." digit+          -- "." required, digits on both sides
bool       = "true" | "false"                 -- case-insensitive
```

The lexer enforces this grammar exactly, not `f64::parse`'s looser rules: `1.` and
`-.5` (digits missing on one side of the dot) are rejected with `QueryError::Parse`,
same as `.5`.

**Type binding:**

- `string` binds as SQL `TEXT` with `COLLATE NOCASE` (case-insensitive
  equality against the property's stored JSON string).
- `integer` binds as `QueryValue::Integer(i64)` -- the full `i64` range
  (`i64::MIN..=i64::MAX`) is supported, including magnitudes beyond `f64`'s
  exact-integer limit of 2^53. An integer lexeme outside `i64` range is a
  **parse-time error** (`QueryError::Parse`), not a silent truncation or a
  fallback to a lossy float.
- `float` binds as `QueryValue::Float(f64)`. A float lexeme whose magnitude
  overflows `f64` to `Infinity` (or otherwise fails to parse as finite) is a
  **parse-time error** (`QueryError::Parse`); non-finite values never reach
  the compiler. The compiler additionally re-checks `is_finite()` on every
  numeric parameter it binds (in `compile_property_equality` and in WHERE
  condition compilation) as defense-in-depth, returning `InvalidInput` if a
  non-finite value is ever constructed by a caller outside the parser (e.g. a
  hand-built AST).
- `bool` binds as `QueryValue::Integer(0)` / `QueryValue::Integer(1)`, no
  `COLLATE`.

**`entity_type` is string-only.** `entity_type` is lifted out of the property
map into `NodePattern.entity_type` and compiled to the dedicated
`entity_type` column (never `json_extract`). A non-string `entity_type`
value (e.g. `{entity_type: 54}`) is rejected at parse time with
`QueryError::Parse`.

**Why integer and float are distinct literal kinds, not one numeric type:**
entity/note properties are stored as JSON, and SQLite's `json_extract`
returns a JSON number as either its INTEGER or REAL storage class depending
on whether the JSON literal had a decimal point. Prior to #832, every numeric
literal parsed to `f64` and compiled to `QueryValue::Float` (`REAL`)
regardless of source form; large integers (`2^53+1` and beyond, including
`i64::MAX`/`i64::MIN`) silently rounded to the nearest representable `f64`
before comparison, so an equality or MATCH-map filter on the *exact* stored
integer could round-trip to a different number and either false-match or
(more commonly) silently match zero rows. Splitting the literal grammar at
parse time -- an integer lexeme has no `.`, a float lexeme requires one --
lets the compiler bind `QueryValue::Integer` for integer literals and
preserve exact `i64` precision through to the SQLite parameter.

**Unsupported forms (rejected, not silently coerced):**

- Scientific notation (`1e10`, `1.5e-3`) is not part of the grammar -- the
  lexer only consumes ASCII digits and a single `.`; an `e`/`E` character
  ends the numeric lexeme and the parser then expects a delimiter (`,` or
  `}`), producing a parse error.
- `null` is not a recognized value literal in either the inline property map
  or WHERE conditions.
- A quoted numeric string (e.g. `{number: '54'}`) is a deliberate `TEXT`
  literal and is never coerced to a number -- it matches JSON strings only,
  not the JSON-number storage class. See the #755 commit for the full
  rationale.

## Invariants and Failure Modes

### Invariants

- **Namespace injection**: `namespace` always comes from `CompileOptions.scopes`,
  never from query text. Bound as a parameter, never as a SQL literal.
- **Edge property whitelist**: only `relation` and `weight` are queryable edge
  columns. Any other property name is rejected.
- **Depth cap**: traversal depth is capped at `MAX_DEPTH` (10 hops). Exceeding it
  is an `InvalidInput` error, not a silent clamp.
- **Pattern shape**: patterns must alternate Node/Edge/Node. Malformed ASTs are
  rejected at validation time.
- **Closed relation taxonomy**: edge relations are validated against
  `EdgeRelation`. Unknown relations are rejected.
- **Synthetic relation closure**: only the four known `observed_as_*` relations
  are valid.

### Failure Modes

- `QueryError::Parse` -- malformed input syntax; also an integer literal
  outside `i64` range or a float literal that overflows to a non-finite
  value (issue #832)
- `QueryError::Validation` -- namespace in query text, unknown relation, inverted
  hop range, malformed pattern shape
- `QueryError::InvalidInput` -- depth exceeds cap, limit overflows `i64`,
  non-finite float parameter (defense-in-depth re-check at compile time; the
  parser already rejects non-finite float literals, issue #832)
- `QueryError::Unsupported` -- zero-hop range, repeated node variable, mixed
  fixed+variable chains, SPARQL `*` paths, OR spanning both endpoints
- `QueryError::Compile` -- empty pattern, unknown variable in RETURN/WHERE, mixed
  synthetic+canonical relations

## Compilation Paths

### Fixed-length (all edges `*1..1`)

Compiles to a JOIN chain:

```text
MATCH (a:concept)-[e:introduced_by]->(b:paper)
->
SELECT ... FROM entities a
JOIN graph_edges e ON e.source_id = a.id
JOIN entities b ON b.id = e.target_id
WHERE ... LIMIT ?
```

### Variable-length (any edge `*N..M` where M > 1)

Compiles to a recursive CTE. Only a single `start_node -[*N..M]-> end_node`
pattern is supported; mixed fixed+variable chains are rejected.

```text
WITH RECURSIVE traverse(...) AS (
    SELECT ... FROM entities s JOIN graph_edges e ... WHERE ...
    UNION ALL
    SELECT ... FROM traverse t JOIN graph_edges e ...
      JOIN entities next_node ...
    WHERE t.depth < ?max_depth AND ... NOT LIKE ...
)
SELECT DISTINCT ... FROM traverse t JOIN entities r ... WHERE ... LIMIT ?
```

## Consistency Notes

- `SPARQL '*'` (zero-or-more hops) is not supported. The recursive CTE seed
  starts at depth 1 and cannot emit a depth-0 row.
- Repeated node variables are rejected at validation. Supporting them requires
  alias-equality predicates not yet implemented.
- `validate_pattern_shape` is called both from `validate_with_warnings` and from
  `compile` to catch hand-constructed malformed ASTs.
- The `parse_auto` fallback for unrecognized prefixes uses the GQL parser.

Last reviewed: 2026-07-10
