# ADR-008: Query Layer as Separate Crate (`khive-query`)

**Status**: accepted\
**Date**: 2026-05-15\
**Authors**: Ocean, lambda:khive

## Context

A research knowledge graph platform needs to support multiple query languages:

1. **SPARQL** (default, W3C standard, lingua franca for RDF/triple-pattern graphs)
2. **GQL** (Graph Query Language, ISO/IEC 39075:2024, modern pattern-matching)
3. **Cypher input** (for migrating data from Neo4j installations)

And multiple compilation targets:

1. **SQL** for SQLite/Postgres backends (compile graph patterns to JOIN trees or recursive CTEs)
2. **Cypher** for Neo4j backend (translate AST to Cypher syntax)
3. **Future**: GremLin, DuckDB graph extensions, etc.

The question: where does parsing + compilation live?

Options:

- **Inside `khive-db`**: Each backend has its own parsers and compilers.
- **Inside `khive-storage`**: Query logic is part of the capability surface.
- **Separate `khive-query` crate**: Backend-agnostic parsing + compilation.

## Decision

**Create a separate `khive-query` crate. Backend-agnostic AST + parsers + compilers.**

```
crates/khive-query/src/
├── lib.rs
├── ast.rs           // Common QueryAST (graph patterns, filters, projections)
├── error.rs         // QueryError (ParseError, CompileError, ValidationError)
├── parsers/
│   ├── mod.rs
│   ├── sparql.rs    // SPARQL 1.1 subset → AST
│   ├── gql.rs       // GQL subset → AST
│   └── cypher.rs    // Cypher input (Neo4j export → AST)
├── compilers/
│   ├── mod.rs
│   ├── sql.rs       // AST → SQL (with Dialect enum: SQLite | Postgres)
│   └── cypher.rs    // AST → Cypher (for Neo4j backend)
└── validate.rs      // Cross-language validation (relation whitelist, depth limits)
```

The crate depends on `khive-storage` for types (`Edge`, `EdgeFilter`, `Direction`, etc.) and
`khive-types` for substrate types. It has **no** dependency on `khive-db` or any backend driver —
parsing and compilation are pure logic.

### Two paths to graph data

After this ADR:

1. **Structured API**: `GraphStore::traverse(TraversalRequest)`, `GraphStore::neighbors(...)`,
   `GraphStore::query_edges(...)`. For programmatic callers who don't need a query language.

2. **Query string API**: `khive_query::parse(language, query_str) -> QueryAST` →
   `khive_query::compile(ast, dialect) -> SqlStatement` → `SqlAccess::query_all(stmt)`. For
   agents/UIs that compose queries dynamically.

Both paths reach the same data through `khive-storage` traits.

## Rationale

### Why a separate crate (not inside db)?

1. **Polyglot frontend**: Three parsers (SPARQL, GQL, Cypher) producing a common AST. Putting them
   inside `khive-db` couples a SQL backend to query syntax it shouldn't know about. Splitting
   parsers across multiple db backend crates duplicates the parsers.

2. **Polyglot backend**: Same AST compiles to SQLite SQL, Postgres SQL, or Neo4j Cypher. The
   compilation logic belongs to neither backend — it's the translation layer.

3. **`khive-storage` is trait-only by design (ADR-005)**. Adding ~1000 LOC of hand-written parsers
   would violate that contract and bloat the dependency graph for every consumer of the trait crate.

4. **Testability**: Query parsing/compilation can be tested independently of any storage backend.
   AST → SQL string comparison is much easier to verify than end-to-end "query → results."

### Why SPARQL as default?

- **Standardized**: SPARQL 1.1 is a W3C recommendation. Stable spec, decades of tooling.
- **Triple pattern model** maps cleanly to entity/edge graphs.
- **Familiar to research community**: bibliographic databases (Wikidata, etc.) speak SPARQL.
- **Federation potential**: SPARQL endpoints can federate queries across services.

GQL is the future ISO standard but tooling is sparse in 2026. We support GQL but lead with SPARQL.

### Why include Cypher input?

The dominant existing graph database is Neo4j. Users migrating from Neo4j have Cypher in their
existing dumps and tooling. Accepting Cypher as an input language reduces migration friction without
forcing them to rewrite queries.

We do NOT need Cypher input parity with Neo4j — just enough to ingest exported subgraphs and
translate familiar pattern syntax to our AST.

### Why Cypher as compilation target?

For Neo4j _interoperability_. If a user runs both khive and Neo4j (e.g., Neo4j as a visualization
frontend, or as a federated graph), they need to translate khive queries → Cypher. The compiler
closes that loop.

This does NOT mean shipping a Neo4j backend in v0.1. It means the architecture supports it when
there's demand.

### Why not just one query language?

Three reasons:

1. Different users have different prior knowledge. Researchers know SPARQL. Engineers from graph
   startups know Cypher. Standards committees push GQL. Picking one excludes large groups.
2. Different use cases favor different languages. Triple-pattern federation is SPARQL's strength;
   pattern matching with shortest-path is Cypher's. GQL is gaining adoption.
3. A common AST means we maintain one execution path, not three. The parser overhead is the cost;
   query power is the benefit.

## Alternatives Considered

| Alternative                                | Pros                                  | Cons                                                           | Why rejected                    |
| ------------------------------------------ | ------------------------------------- | -------------------------------------------------------------- | ------------------------------- |
| Parsers inside `khive-db`                  | One fewer crate                       | Couples storage to query syntax; can't reuse for Neo4j backend | Wrong abstraction               |
| One language only (SPARQL)                 | Less work                             | Excludes Cypher/GQL users; no Neo4j migration story            | Loses key user segments         |
| External crates (`sparql-rs`, `cypher-rs`) | No DIY parsing                        | None mature enough for our subset; no AST sharing              | Build vs buy went the wrong way |
| Parser + compiler in `khive-storage`       | Single crate for all storage concerns | Bloats trait crate with implementations                        | ADR-005 violation               |

## Consequences

### Positive

- Query languages can be added/changed without touching backends.
- Backends can be added/changed without touching query languages.
- AST is the stable contract — parsers and compilers evolve independently.
- New backend (e.g., DuckDB graph extension) just needs an AST→target compiler.
- Tests are fast (no IO required for parser/compiler tests).

### Negative

- One more crate to maintain. Mitigated: clear boundaries, single responsibility.
- AST design decisions are upfront and have long-term consequences. Mitigated: derived from observed
  research-KG query patterns (see Worked Examples below).

### Neutral

- Backend crates may bundle their own SQL helpers (e.g., dialect-specific string escaping) that
  don't need to live in `khive-query`.

## Implementation Plan

### Phase 1 (v0.1, shipping today)

- ADR documented (this file).
- Structured `GraphStore::traverse` API works (already implemented in `khive-db`).
- `khive-query` crate **not built yet** — structured API is sufficient for v0.1 demo.

### Phase 2 (v0.2)

- Build out GQL parser + SQL compiler in `crates/khive-query/`.
- Build SPARQL parser in the same crate.
- Wire MCP server to expose `query(language, q)` tool.
- Tests: parser correctness, AST round-trips, SQL compilation correctness.

### Phase 3 (v0.3+)

- Cypher input parser (for Neo4j migration).
- Cypher output compiler (for Neo4j interop / federation).
- Federation: query routing across multiple backends.

## Validation Rules in `khive-query`

The query layer enforces:

1. **Closed edge ontology (ADR-002)**: Reject queries that name relations outside the 13 canonical
   set.
2. **Depth limits**: Cap traversal depth (default 5, max 10) to prevent runaway recursion.
3. **Namespace scoping**: Inject `WHERE namespace = ?` from the calling context — never trust query
   strings to set namespaces.
4. **Read-only**: The query layer is for reads. Writes go through structured `GraphStore` methods.

## References

- ADR-002: Closed Edge Ontology (validation rules)
- ADR-005: Storage Capability Traits (`GraphStore`, `SqlAccess` — execution layer)
- ADR-009: Backend Portability (compilation targets)
