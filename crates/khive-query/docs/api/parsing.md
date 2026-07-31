# GQL and SPARQL Parsing

The parsers translate two read-only query syntaxes into the shared `GqlQuery` AST. Both are hand-written recursive-descent parsers and reject write-shaped input before normal parsing so callers receive an actionable read-only error.

## Language dispatch

`parse_in_language(input, language)` invokes the selected parser directly. `parse_auto(input)` first runs the unified write guard, then chooses SPARQL for a leading `SELECT`, GQL for `MATCH`, and GQL as the compatibility fallback for other prefixes.

The unified guard recognizes GQL/Cypher mutations and SPARQL Update forms, including `WITH ... DELETE` and updates preceded by `PREFIX` or `BASE`. Direct GQL and SPARQL parser entry points retain their own guards as defense in depth.

## GQL parser

`parsers::gql::parse` accepts a `MATCH` pattern, optional `WHERE`, `RETURN`, and optional `LIMIT`. Patterns support directed or undirected edges, relation alternatives, inline node properties, and bounded hop ranges.

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

`parsers::sparql::parse` accepts the crate's SPARQL-inspired `SELECT ... WHERE { ... } LIMIT ...` subset and converts triples into the same alternating path AST. Predicate paths support one hop, `+`, and explicit inclusive ranges.

A numeric predicate condition targets a JSON property explicitly, even when its predicate name matches a dedicated field. This matches string-valued predicate properties and removes the prior value-type-dependent field ambiguity.

The AST currently represents one connected, non-branching path. Disconnected or branched edge triples, and kind/property constraints on variables outside that path, are rejected so no conjunct is silently discarded. Triple conditions are folded into a left-associative `AND` tree.

SPARQL `*` is rejected: it means zero-or-more, while the recursive SQL seed begins at depth one and cannot emit the start node as a depth-zero result. Treating `*` as `+` would lose valid matches.

`leading_keyword` skips whitespace, `#` line comments, and repeated `PREFIX`/`BASE` prologue declarations before returning the operative keyword used by the read-only guards.

## Parse errors and unsupported forms

Malformed tokens, unterminated strings, trailing input, invalid numeric forms, and grammar mismatches return `QueryError::Parse`. Recognized writes or semantics such as SPARQL zero-hop paths return `QueryError::Unsupported`. Neither parser executes SQL or mutates an input AST.
