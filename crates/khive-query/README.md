# khive-query

GQL and SPARQL parsers with a SQL compiler for knowledge graph queries. Both
dialects parse into a single shared `GqlQuery` AST, which one compiler lowers to
parameterized SQL.

## Usage

```rust
use khive_query::{compile, parse_auto, CompileOptions};

// parse_auto detects the dialect: `SELECT` -> SPARQL, `MATCH` -> GQL, else GQL.
let query = parse_auto("MATCH (a:concept)-[:extends]->(b:concept) RETURN a, b")?;

let opts = CompileOptions {
    scopes: vec!["local".to_string()],
    max_limit: 500,
};
let compiled = compile(&query, &opts)?;
// compiled.sql: parameterized SQL string
// compiled.params: Vec<QueryValue> bound positionally
// compiled.return_vars, compiled.warnings
```

Language can also be selected explicitly:

```rust
use khive_query::{parse, QueryLanguage};

let query = parse(QueryLanguage::Sparql, "SELECT ?a WHERE { ?a :extends ?b . }")?;
```

## Architecture

```text
input string ─┬─ parse_auto() ──dispatch by leading keyword──┬─ parsers::gql::parse()
              └─ parse(lang, .) ─────────────────────────────┘  parsers::sparql::parse()
                                        │
                                    GqlQuery (ast.rs)
                                        │
                             validate::validate_with_warnings()
                                        │
                              compilers::sql::compile()
                                        │
                              CompiledQuery { sql, params, return_vars, warnings }
```

`validate_with_warnings` runs structural checks (pattern must alternate Node/Edge/Node,
traversal depth capped at `MAX_DEPTH` = 10 hops) before the compiler emits SQL, and the
compiler additionally asserts the emitted statement is `SELECT`/`WITH`-only as a
defense-in-depth guard. Both the SPARQL and GQL entry points reject write-shaped input
(`CREATE`, `DELETE`, `INSERT`, `SPARQL Update` forms, …) before AST construction —
`parse_auto` also runs this guard first, so forms like `WITH <g> DELETE …` that don't
start with a dialect keyword are still caught. `khive-query` never emits mutating SQL;
graph writes go through `create` / `update` / `link` / `merge` / `delete` at the
runtime layer instead.

`CompileOptions.scopes` restricts the query to specific namespaces (empty means
cross-namespace); `max_limit` is a server-side cap — the effective `LIMIT` is
`min(requested, max_limit)`.

Errors are typed as `QueryError`: `Parse { position, message }`, `Compile(String)`,
`Validation(String)`, `Unsupported(String)`, `InvalidInput(String)`.

## Where this sits

`khive-query` depends only on `khive-types` and sits between the storage layer and the
runtime:

```text
types -> score -> storage -> db -> query -> runtime -> pack-* -> mcp
```

`khive-runtime` calls `parse_auto`/`parse` and `compile` to serve the `query` verb
(GQL/SPARQL pattern matching), then executes the resulting `CompiledQuery` through
`khive-db`'s `SqlAccess`. Parser/validator/compiler separation is governed by
[ADR-008](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-008-query-layer-separation.md).

## License

Apache-2.0.
