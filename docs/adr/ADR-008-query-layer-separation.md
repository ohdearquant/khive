# ADR-008: Query Layer Separation

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: khive maintainers

## Context

khive supports structured graph queries through two query languages: GQL (Graph Query
Language) and SPARQL. These are distinct from the verb-dispatch DSL (`khive-request`) that
routes MCP requests to verb handlers.

The query layer must satisfy:

1. **Read-only compilation.** Graph query languages compile to SQL `SELECT` statements.
   Mutations go through verb handlers, not query strings.
2. **Backend independence.** The query compiler targets SQL. It has no driver dependency on
   SQLite, no ATTACH awareness, and no backend topology context.
3. **Relation validation.** The 15 canonical edge relations (ADR-002) are validated by
   delegating to `EdgeRelation::from_str`. The query layer does not maintain its own
   relation allowlist.
4. **`entity_type` as first-class field.** ADR-001 settles `entity_type` as a dedicated
   indexed column. Query AST and SQL compilation must treat it as a column predicate, not
   as a JSON property extraction.

## Decision

### Two query languages, one compilation target

```text
Input:  GQL, SPARQL
Output: SQL (single-database)
```

GQL and SPARQL are the shipped query frontends. Both compile to SQL through a shared AST.

Cypher is removed from the normative architecture. No Cypher parser, compiler, or output
dialect exists in the codebase. The original Neo4j interop rationale is retained under
Alternatives Considered. A future Cypher frontend requires a new ADR triggered by a
concrete use case.

### Crate structure

`khive-query` is a separate crate from `khive-request`. They do not share grammar, AST,
or validation responsibilities.

```text
khive-request — parses verb-dispatch DSL: create(...), search(...), [v1(...), v2(...)]
khive-query   — parses graph query language strings: MATCH (n)-[r]->(m), SELECT ?s ?p ?o
```

### Dispatch sequence

```text
MCP request
  ↓
khive-request parses verb-dispatch DSL
  ↓
VerbRegistry dispatches to `query` verb handler
  ↓
khive-query parses GQL/SPARQL query string
  ↓
khive-query validates AST (relation names, depth limits)
  ↓
khive-query compiles AST → SqlStatement
  ↓
SqlAccess::query_all(stmt)
```

The query layer sits below the verb handler. It receives a query string and returns a
compiled SQL statement. It does not interact with MCP, the VerbRegistry, or storage
directly.

### AST: `entity_type` is first-class

The query AST stores `entity_type` as a dedicated field, not as a property predicate:

```rust
pub struct NodePattern {
    pub var: Option<String>,
    pub kind: Option<String>,
    pub entity_type: Option<String>,
    pub properties: HashMap<String, Literal>,
}
```

The SQL compiler maps these to column predicates:

```text
kind        → entities.kind = ?
entity_type → entities.entity_type = ?
properties  → JSON property predicates
```

The query layer does not own `EntityTypeRegistry` validation. Write-time validation
remains in `khive-runtime` (ADR-001, ADR-003). Query-time filtering by an unknown
`entity_type` simply returns no rows unless the runtime normalizes the query before
compilation.

### Relation validation

`khive-query` validates relation names by delegating to `EdgeRelation::from_str` from
`khive-types`. It does not maintain its own relation allowlist.

Adding a new relation to `khive-types::EdgeRelation` (e.g., `precedes`, `derived_from`)
extends accepted query relations automatically. The regression suite must include positive
and negative parser/validator cases for every canonical relation.

Endpoint validation (which `(source_kind, relation, target_kind)` triples are legal) is
NOT a query-layer concern. It lives in `khive-runtime` (ADR-002, ADR-003). The query
compiler does not reject a query because the relation is used between unexpected kinds —
it compiles the pattern and returns empty results if no matching edges exist.

### Single-database compilation

The SQL compiler targets one logical SQL database. It does not implement:

- SQLite ATTACH qualification
- Cross-backend query planning
- SPARQL `SERVICE` federation
- Schema-prefix generation

If cross-backend query-language federation is added later, it must introduce an explicit
compile target or relation-binding model rather than ad-hoc string schema prefixes. The
SubstrateCoordinator (ADR-003, ADR-029 (Substrate Coordinator)) handles cross-backend fan-out above the query
compiler.

### Depth limits

Graph traversal queries have a maximum depth of 10:

```rust
pub const MAX_TRAVERSAL_DEPTH: usize = 10;
```

The compiler rejects queries exceeding this depth at AST validation time.

### GQL WHERE expression

GQL `WHERE` clauses support `AND` and `OR` expression nodes plus `=`, `!=`, `>`, `<`,
`>=`, `<=`, `LIKE`, `CONTAINS`, `STARTS WITH`, `IN` list literals, and `IS NOT NULL`
predicates. `CONTAINS` and `STARTS WITH` compile to escaped, parameterized `LIKE`
predicates. `IN` list items may be string, integer, finite float, or boolean scalars, and
lists may mix those types. Values are individually bound, and a list containing any string
uses case-insensitive string collation. An empty list compiles to match nothing without
binding value parameters. `NULL` is not a valid list item and is rejected during parsing.
Without `OR`/`IN`, multi-value filters require N separate queries or caller-side UNION.

### Read-only constraint

`khive-query` compiles read-only SQL. It does not generate `INSERT`, `UPDATE`, `DELETE`,
or DDL statements. Mutations go through verb handlers (`create`, `update`, `delete`,
`link`) which call runtime operations, not the query compiler.

The `query` verb handler may share infrastructure with other verbs (e.g., filter parsing),
but the compilation path produces `SELECT` statements only.

This invariant is enforced at two independent levels:

1. **Parser-level guard.** `parsers::gql::parse` and `parsers::sparql::parse` each check the
   leading keyword before parsing. SPARQL write operations (`INSERT`, `DELETE`, `LOAD`,
   `CLEAR`, `DROP`, `ADD`, `MOVE`, `COPY`, `CREATE`) and GQL/Cypher write forms (`CREATE`,
   `DELETE`, `SET`, `REMOVE`, `MERGE`, `INSERT`, `UPDATE`) are rejected with an explicit
   `Unsupported` error that names the mutation verbs to use instead:
   `"the query verb is read-only; to mutate the graph use: create, update, link, merge, delete"`.

2. **Compiler-level guard (`assert_select_only`).** After the SQL string is built, the
   compiler asserts it starts with `SELECT` or `WITH` (recursive CTE). This is defense-in-depth
   against a future code path that somehow bypasses the parser check.

Both levels are covered by regression tests in `crates/khive-query/src/`.

## Rationale

### Why separate crate from khive-request?

`khive-request` parses a verb-dispatch DSL with function-call syntax. `khive-query` parses
graph query languages (GQL, SPARQL) with their own grammars. They have different parsers,
different ASTs, different validation rules, and different output shapes. Merging them would
couple verb dispatch to graph query grammar changes.

### Why no Cypher?

No Cypher implementation exists. Listing it as a planned frontend creates a roadmap
obligation that is not justified by any current use case. GQL covers the same graph
pattern matching needs. If Neo4j interop becomes a concrete requirement, a new ADR can
introduce Cypher at that time.

### Why entity_type as first-class (not JSON)?

ADR-001 defines `entity_type` as a dedicated indexed column. Emitting
`json_extract(properties, '$.type')` would bypass the index, produce different query
plans, and contradict the schema decision. `entity_type` as a column predicate gets index
support and correct filtering.

### Why single-database (not ATTACH-aware)?

ATTACH schema aliases are coordinator binding metadata (ADR-005, ADR-007). The query
compiler should not know about backend topology. The coordinator builds schema-qualified
table names when needed; the query compiler produces unqualified SQL that works against
any single database.

### Why read-only?

Write operations require validation (entity_type normalization, endpoint legality,
namespace enforcement) that lives in the runtime (ADR-003). If the query compiler could
generate writes, it would need access to the EntityTypeRegistry and endpoint validator —
violating the separation between syntax compilation and semantic validation.

## Alternatives Considered

| Alternative                          | Why rejected                                                                                            |
| ------------------------------------ | ------------------------------------------------------------------------------------------------------- |
| Cypher frontend                      | No implementation exists. No concrete use case. GQL covers graph patterns.                              |
| Cypher output dialect                | No Neo4j backend exists. SQL is the only compilation target.                                            |
| ATTACH-aware compiler                | Topology is a coordinator concern. Query compiler stays backend-blind.                                  |
| entity_type via JSON extraction      | Bypasses the dedicated indexed column (ADR-001). Wrong query plan.                                      |
| Endpoint validation in query layer   | Semantic validation belongs in runtime (ADR-003). Query compiles patterns; runtime validates semantics. |
| Merge khive-query into khive-request | Different grammars, ASTs, and concerns. Coupling creates unnecessary churn.                             |

## Consequences

### Positive

- Query crate compiles against `khive-types` only — no runtime, storage, or driver dependency.
- `entity_type` queries use the dedicated index.
- Relation validation is automatic via `EdgeRelation::from_str`.
- Adding a new relation to `khive-types` requires zero query-crate changes.
- Read-only constraint prevents query-path bypass of runtime validation.

### Negative

- Two parser crates (`khive-request`, `khive-query`) is more surface than one.
  Mitigated: they solve different problems with different grammars.
- Single-database compilation means cross-backend queries require coordinator fan-out
  above the query layer.
  Mitigated: this is the correct architectural boundary per ADR-003.

### Neutral

- `QueryLanguage` enum retains `Gql` and `Sparql` variants. No enum migration needed.
- Max traversal depth of 10 is a safety bound, not a feature constraint.

## Implementation

- `crates/khive-query/src/parsers/gql.rs`: GQL parser.
- `crates/khive-query/src/parsers/sparql.rs`: SPARQL parser.
- `crates/khive-query/src/ast.rs`: shared AST with `NodePattern.entity_type` field.
- `crates/khive-query/src/validate.rs`: AST validation (relation via `EdgeRelation::from_str`,
  depth limits).
- `crates/khive-query/src/compilers/sql.rs`: AST → SQL compilation. `entity_type` maps to
  column predicate.
