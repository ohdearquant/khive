# GQL and SPARQL Parsing

The parsers translate two read-only query syntaxes into the shared `GqlQuery` AST. Both are hand-written recursive-descent parsers and reject write-shaped input before normal parsing so callers receive an actionable read-only error.

## Language dispatch

`parse_in_language(input, language)` invokes the selected parser directly. `parse_auto(input)` first runs the unified write guard, then chooses SPARQL for a leading `SELECT`, GQL for `MATCH`, and GQL as the compatibility fallback for other prefixes.

The unified guard recognizes GQL/Cypher mutations and SPARQL Update forms, including `WITH ... DELETE` and updates preceded by `PREFIX` or `BASE`. Direct GQL and SPARQL parser entry points retain their own guards as defense in depth.

## GQL parser

`parsers::gql::parse` implements the following read-only dialect. Keywords are case-insensitive; the grammar accepts exactly one connected, alternating path after `MATCH`.

```text
query       = "MATCH" path ["WHERE" where_expr]
              "RETURN" return_item ("," return_item)*
              ["SKIP" integer] ["LIMIT" integer]
path        = node (edge node)*
node        = "(" [identifier] [":" identifier] [property_map] ")"
property_map = "{" [property ("," property)*] "}"
property    = identifier ":" value
edge        = "-[" edge_body "]->" | "<-[" edge_body "]-"
            | "-[" edge_body "]-"  | "<-[" edge_body "]->"
edge_body   = [identifier] [":" identifier ("|" identifier)*] [hop_range]
hop_range   = "*" | "*" integer | "*" integer ".." integer
return_item = identifier ["." identifier]
```

The four edge spellings represent outgoing, incoming, and the two accepted undirected forms. An omitted relation matches any supported edge relation. A bare `*` means one through five hops; explicit ranges are inclusive and validation caps the maximum at ten. `SKIP` and `LIMIT` take non-negative integers; when both are present, `SKIP` precedes `LIMIT`.

Only unaliased variables and `variable.property` projections are supported. `AS`, expressions, aggregates, `DISTINCT`, caller-defined ordering, and `OFFSET` are outside this dialect. The compiler supplies a deterministic identity order so `SKIP` pages do not overlap while the matched data is unchanged. Result column names are derived from their variables, such as `a_id` and `e_relation`.

### One-path boundary and parallel-edge alternative

Comma-separated `MATCH` patterns are not supported. This form returns an unsupported-feature error that names the construct:

```text
MATCH (a)-[e1]->(b), (a)-[e2]->(b) RETURN a, b
```

To audit parallel edges, list edge records with one unlabeled-edge path instead:

```text
MATCH (a)-[e]->(b) RETURN a.id, e.id, e.relation, b.id
```

Group rows by `a_id` and `b_id` client-side, while retaining `e_id` and `e_relation` so endpoint grouping does not erase edge identity. This enumerates edge records; it does not add a self-join or grouping operation to the query language.

Return aliases are likewise recognized and rejected with an unsupported-feature error that names `AS`:

```text
MATCH (a)-[e]->(b) RETURN a.name AS src, b.name AS dst LIMIT 5
```

The `WHERE` grammar gives `AND` tighter precedence than `OR`:

```text
where_expr = and_expr ("OR" and_expr)*
and_expr   = condition ("AND" condition)*
```

Conditions support `=`, `!=`, `>`, `<`, `>=`, `<=`, `LIKE`, `CONTAINS`, `STARTS WITH`, `IN [...]`, and `IS NOT NULL`. `CONTAINS` and `STARTS WITH` treat `%`, `_`, and `\` literally; the compiler escapes them before adding its own wildcard.

A condition references either a dedicated field (`n.name`) or an explicit JSON-property path (`n.properties.finding.severity`). JSON paths require one or more identifier segments after `properties`; another dotted root such as `n.finding.severity` is a parse error. A single unknown field such as `n.severity` parses as a field reference and is rejected during substrate-aware compilation with the valid direct fields and the `properties.<path>` guidance.

### Literal grammar

Inline maps and `WHERE` conditions share this scalar grammar:

```text
value   = string | integer | float | bool
integer = ["-"] digit+
float   = ["-"] digit+ "." digit+
bool    = "true" | "false"       # case-insensitive
```

Digits are required on both sides of a float's decimal point, so `.5`, `1.`, and `-.5` are errors. Scientific notation and `null` are unsupported rather than silently coerced. Integers outside `i64` and floats that parse as non-finite are parse errors. `entity_type` in an inline map must be a string and is lifted into `NodePattern.entity_type`.

## SPARQL parser

`parsers::sparql::parse` accepts the crate's SPARQL-inspired `SELECT ... WHERE { ... } LIMIT ...` subset and converts triples into the same alternating path AST. Predicate paths support one hop, `+`, and explicit inclusive ranges. SPARQL `OFFSET` remains unsupported; its shared AST offset is always zero. Issue #1601 intentionally adds paging only to the GQL surface.

A numeric predicate condition targets a JSON property explicitly, even when its predicate name matches a dedicated field. This matches string-valued predicate properties and removes the prior value-type-dependent field ambiguity.

The AST currently represents one connected, non-branching path. Disconnected or branched edge triples, and kind/property constraints on variables outside that path, are rejected so no conjunct is silently discarded. Triple conditions are folded into a left-associative `AND` tree.

SPARQL `*` is rejected: it means zero-or-more, while the recursive SQL seed begins at depth one and cannot emit the start node as a depth-zero result. Treating `*` as `+` would lose valid matches.

`leading_keyword` skips whitespace, `#` line comments, and repeated `PREFIX`/`BASE` prologue declarations before returning the operative keyword used by the read-only guards.

## Parse errors and unsupported forms

Malformed tokens, unterminated strings, trailing input, invalid numeric forms, and grammar mismatches return `QueryError::Parse`. Recognized unsupported semantics—including comma-separated GQL `MATCH` patterns, GQL `AS` aliases, writes, and SPARQL zero-hop paths—return `QueryError::Unsupported`. Neither parser executes SQL or mutates an input AST.
