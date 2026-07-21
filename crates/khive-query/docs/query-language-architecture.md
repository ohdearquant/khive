# Query Language Architecture

Backend-agnostic GQL/SPARQL parsing and SQL compilation crate. Parses query text
into a shared `GqlQuery` AST, validates edge relations against the closed
`EdgeRelation` taxonomy, and compiles the AST to parameterized SQL for execution
by the runtime. The crate depends only on `khive-types` (for `EdgeRelation`); it
has no dependency on storage, DB, or runtime crates.

## ADR Links

- ADR-001: Entity Kind Taxonomy
- ADR-002: Closed Edge Ontology
- ADR-008: Query Layer Separation
- ADR-041: Event Provenance Projection — Hybrid Log + Graph Edges

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

The dependency boundary is intentional: `QueryValue` mirrors only the storage
values needed by compilation, and the runtime performs the final conversion.
That keeps parsing and compilation reusable without depending on a database or
storage implementation. The complete item-level contracts live in
[`docs/api/ast.md`](api/ast.md), [`docs/api/parsing.md`](api/parsing.md),
[`docs/api/validation.md`](api/validation.md), and
[`docs/api/sql-compilation.md`](api/sql-compilation.md).

### Synthetic Observation Edge Paths (ADR-041)

Synthetic observation relations project event provenance without materializing
duplicate graph edges. They therefore compile through `event_observations`,
while canonical relations continue to use `graph_edges`. Keeping those paths
separate preserves the closed edge ontology and makes the provenance projection
read-only. Exact role, substrate, direction, and projection rules are documented
in [`docs/api/sql-compilation.md`](api/sql-compilation.md).

### Literal fidelity (issues #755 and #832)

The AST distinguishes text, integer, decimal, and boolean literals because
SQLite JSON extraction preserves those storage classes. This avoids both the
old text-versus-number mismatch and the `f64` rounding of large integers. The
accepted grammar and failure behavior are documented in
[`docs/api/parsing.md`](api/parsing.md) and [`docs/api/ast.md`](api/ast.md).

## Compilation strategy

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

## Deliberate limitations

- `SPARQL '*'` (zero-or-more hops) is not supported. The recursive CTE seed
  starts at depth 1 and cannot emit a depth-0 row.
- Repeated node variables are rejected at validation. Supporting them requires
  alias-equality predicates not yet implemented.
- The `parse_auto` fallback for unrecognized prefixes uses the GQL parser.

Last reviewed: 2026-07-10
