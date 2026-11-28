# GQL and SPARQL Parsing

The parsers translate two read-only query syntaxes into the shared `GqlQuery` AST. Both are hand-written recursive-descent parsers and reject write-shaped input before normal parsing so callers receive an actionable read-only error.

## Language dispatch

`parse_in_language(input, language)` invokes the selected parser directly. `parse_auto(input)` first runs the unified write guard, then chooses SPARQL for a leading `SELECT`, GQL for `MATCH`, and GQL as the compatibility fallback for other prefixes.

The unified guard recognizes GQL/Cypher mutations and SPARQL Update forms, including `WITH ... DELETE` and updates preceded by `PREFIX` or `BASE`. Direct GQL and SPARQL parser entry points retain their own guards as defense in depth.

## GQL parser

`parsers::gql::parse` implements the following read-only dialect. Keywords are case-insensitive; comma-separated alternating paths share one global tail. Validation requires multiple paths to be connected and each edge to be canonical and fixed at one hop.

```text
query       = "MATCH" path ("," path)* ["WHERE" where_expr]
              "RETURN" ["DISTINCT"] return_item ("," return_item)*
              ["ORDER BY" order_item ("," order_item)*]
              ["SKIP" integer] ["LIMIT" integer]
path        = node (edge node)*
node        = "(" [identifier] [":" identifier] [property_map] ")"
property_map = "{" [property ("," property)*] "}"
property    = identifier ":" value
edge        = "-[" edge_body "]->" | "<-[" edge_body "]-"
            | "-[" edge_body "]-"  | "<-[" edge_body "]->"
edge_body   = [identifier] [":" identifier ("|" identifier)*] [hop_range]
hop_range   = "*" | "*" integer | "*" integer ".." integer
return_item = identifier | reference | "count" "(" identifier ")"
order_item  = reference ["ASC" | "DESC"]
reference   = identifier "." identifier
            | identifier ".properties." identifier ("." identifier)*
            | identifier ".metadata." identifier ("." identifier)*
```

The four edge spellings represent outgoing, incoming, and the two accepted undirected forms. An omitted relation matches any supported edge relation. A bare `*` means one through five hops; explicit ranges are inclusive and validation caps the sum of variable-segment maxima at ten. A single canonical path may combine fixed steps and multiple variable segments; fixed `1..1` steps do not consume that budget or contribute to traversal metrics. `SKIP` and `LIMIT` take non-negative integers; when both are present, `SKIP` precedes `LIMIT`.

Unaliased variables, `variable.property`, `variable.properties.<path>` and `edge.metadata.<path>` projections are supported. `RETURN DISTINCT` and `count(binding)` are supported. `AS`, general expressions, other aggregates, `count(*)`, `count(DISTINCT ...)`, property count operands, and `OFFSET` remain outside this dialect. `ORDER BY` accepts supported scalar fields and explicit JSON paths, with `ASC` as the default direction. It rejects bare containers, namespace fields, aliases, functions, and `NULLS` clauses. The compiler appends its deterministic tie order so `SKIP` pages do not overlap while the matched data is unchanged. Without `ORDER BY`, ordinary projections retain the previous compiler order; explicit-distinct and grouped results use their returned values for deterministic ties. Result column names are derived from their variables, such as `a_id` and `e_relation`.

`RETURN e.metadata` on a canonical edge projects its stored JSON text or SQL null as `e_metadata`. Returning the whole edge variable still expands to its existing fields without metadata: five for fixed-length matches and three for recursive matches. Dotted metadata leaves can be returned as typed scalar or JSON values; `RETURN e.metadata.rank.order` is named `e_metadata_rank_order`. Node-property paths use the same form, such as `n.properties.status` named `n_properties_status`. Synthetic observation edges have no canonical metadata and reject metadata projections, predicates and ordering. Every returned path requires at least one validated identifier after its root. Different expressions producing the same exact output name are refused; `n.properties.a_b` and `n.properties.a.b` both produce `n_properties_a_b`. Existing direct fields and whole-container projections keep their names and shape.

`count` is recognized as an aggregate only when followed by `(`. Bindings such as `count` and `count.id` retain their ordinary meaning. Its operand must resolve to one bound node or edge. The output name is `count_<binding>` and participates in the same exact-case collision check as other projections. Counts operate on matching rows before recursive projection deduplication; other returned values form implicit groups. Counts-only empty inputs return one zero row, while empty inputs with grouping keys return none. Explicit distinct/grouped results omit implicit recursive metrics, reject hidden ordering expressions, and page only after grouping/deduplication. No `GROUP BY`, `HAVING`, count-function ordering or generated-alias ordering syntax is added.

### Connected paths and edge identity

Comma-separated paths share named node identities. This form matches two independently bound edges around the same typed endpoints:

```text
MATCH (a)-[e1]->(b), (a)-[e2]->(b) RETURN a, b
```

The shared nodes must agree on substrate and UUID. Every occurrence's kind, subtype and inline-property constraints applies; references use the first occurrence. Edge binding names must be distinct, but their selected records need not be: two matching edges produce four edge pairs, including the two same-edge pairs. Equal projected rows retain their multiplicity unless explicit DISTINCT or a count grouping collapses them. Repeated node names within one path and node/edge name collisions remain refused.

All components must connect through shared named nodes. A later component may bridge earlier components; anonymous occurrences cannot connect them. Multiple fixed one-hop edges within a component are supported. Disconnected components, recursive or synthetic multi-pattern matches return `QueryError::Unsupported` during validation/compilation. Empty or trailing comma components are parse errors.

To enumerate edge records rather than combinations of edges, use one unlabeled-edge path:

```text
MATCH (a)-[e]->(b) RETURN a.id, e.id, e.relation, b.id
```

Group rows by `a_id` and `b_id` client-side, while retaining `e_id` and `e_relation` so endpoint grouping does not erase edge identity. This enumerates edge records without multiplying matches across paths; client-side grouping is useful when retaining the individual edge rows; `RETURN a.id, b.id, count(e)` instead requests server-side group counts.

Return aliases are likewise recognized and rejected with an unsupported-feature error that names `AS`:

```text
MATCH (a)-[e]->(b) RETURN a.name AS src, b.name AS dst LIMIT 5
```

The `WHERE` grammar gives `AND` tighter precedence than `OR`:

```text
where_expr = and_expr ("OR" and_expr)*
and_expr   = where_atom ("AND" where_atom)*
where_atom = condition | "NOT EXISTS" "{" path "}"
```

Conditions support `=`, `!=`, `>`, `<`, `>=`, `<=`, `LIKE`, `CONTAINS`, `STARTS WITH`, `IN [...]`, and `IS NOT NULL`. `CONTAINS` and `STARTS WITH` treat `%`, `_`, and `\` literally; the compiler escapes them before adding its own wildcard.

A condition references a dedicated field (`n.name`), an explicit node JSON-property path (`n.properties.finding.severity`), or a canonical-edge metadata path (`e.metadata.order`). JSON paths require one or more identifier segments after their root; another dotted root such as `n.finding.severity` is a parse error. A single unknown field such as `n.severity` parses as a field reference and is rejected during substrate-aware compilation with the valid direct fields and JSON-path guidance. Node metadata, edge properties, and bare metadata/property containers in predicates are unsupported. A metadata key named `namespace` is ordinary data; direct namespace fields remain forbidden.

Ordering uses SQLite's typed values and BINARY text collation, whereas text equality predicates retain their case-insensitive behavior. For each variable-length segment, edge metadata refers to that segment's final traversed edge; fixed-edge bindings keep their own metadata. A recursive, explicit-distinct or grouped order term must be derivable from returned projections: `RETURN e.metadata ORDER BY e.metadata.order` and `RETURN e.metadata.order ORDER BY e.metadata.order` succeed, while `RETURN e ORDER BY e.metadata.order` returns an unsupported-feature error because whole-edge projections omit metadata. A returned leaf does not expose a different hidden path. Generated output labels are not accepted as ordering references. For a single variable segment, an `OR` combining start-node and final-row predicates remains unsupported; final-edge and end-node predicates can share an `OR`. A mixed canonical path evaluates the full Boolean tree on the complete joined row, so its fixed and variable bindings may share an `OR` before grouping and paging.

### Correlated absence

One absence atom checks whether a canonical fixed path has no visible match:

```text
MATCH (r:concept {role: 'requirement'})
WHERE NOT EXISTS { (f:concept)-[:supports]->(r) }
RETURN r.name
```

At least one inner node name must already bind an outer node. Correlation uses substrate and UUID, with all kind, subtype and inline-property constraints still applied. A correlated singleton such as `NOT EXISTS { (r:note) }` is valid; an empty or uncorrelated block is refused. Inner edges must be canonical and exactly one hop each, though a block may contain several such steps. Incoming and undirected steps and relation alternatives retain their ordinary meaning.

Additional names are local to the block and can be reused in independent blocks. They cannot appear in outer predicates, RETURN, count operands or ORDER. Reusing an outer edge variable as an inner edge refuses with a diagnostic naming it and stating that edge variables are not correlated into NOT EXISTS; there is no silent shadowing. Node/edge name conflicts and repeated names within one path remain refused.

`NOT EXISTS` is contextual: `not.name` and `exists.name` remain scalar references. It composes with scalar atoms under the existing AND-before-OR precedence. A block has no inner MATCH, WHERE, RETURN, ORDER, comma list or nested absence syntax. OPTIONAL MATCH, positive EXISTS, recursive inner edges and synthetic inner edges remain unsupported. Existing synthetic outer node bindings can still correlate to a canonical inner path.

For a single-variable-segment outer path, start-only absence belongs to the seed phase; any block using the end node belongs to the final row, including a block using both endpoints. AND can combine phases, while OR across seed and final phases remains refused. On a mixed path, absence can correlate to any named outer node, including intermediate bindings, and is evaluated with the complete outer WHERE. Absence filtering precedes grouping, DISTINCT and pagination.

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

`parsers::sparql::parse` accepts the crate's SPARQL-inspired `SELECT ... WHERE { ... } LIMIT ...` subset and converts triples into the same alternating path AST. Predicate paths support one hop, `+`, and explicit inclusive ranges. SPARQL `OFFSET` remains unsupported; its shared AST offset is always zero, `distinct` is false and its order list is empty. This addition does not extend SPARQL projection syntax. GQL ordering keywords do not change SPARQL identifiers named `order`, `by`, `asc`, or `desc`. Issue #1601 intentionally adds paging only to the GQL surface.

A numeric predicate condition targets a JSON property explicitly, even when its predicate name matches a dedicated field. This matches string-valued predicate properties and removes the prior value-type-dependent field ambiguity.

The SPARQL parser still produces one connected, non-branching path with an empty additional-pattern vector. Disconnected or branched edge triples, and kind/property constraints on variables outside that path, are rejected so no conjunct is silently discarded. Triple conditions are folded into a left-associative `AND` tree.
Repeated kind triples may pair one substrate label (`entity`, `note`, `event`, or `edge`) with
one granular kind label; both constrain the same node regardless of triple order. Different
labels within either class are parse errors. Repeated string-property triples are accepted as one
constraint when their values compare equal under the compiler's collation: ASCII case-insensitive
for ordinary string properties, exact for `entity_type`. Conflicting kind labels or repeated
string-property values for the same subject and predicate are parse errors rather than silently
replacing an earlier constraint. Numeric-property triples remain separate JSON-property equality
conditions in the `AND` tree, including when the same predicate has different numeric values.
Because each JSON property is scalar, contradictory numeric equalities return no rows; they are
not parse errors. A string and a numeric triple on the same predicate are likewise both retained.

SPARQL `*` remains unsupported unbounded zero-or-more syntax. Existing explicit `{min,max}` ranges may include zero, including `{0,0}` and `{0,1}`; they use the shared typed zero-hop semantics and maximum depth cap. Treating bare `*` as `+` would still lose valid matches.

`leading_keyword` skips whitespace, `#` line comments, and repeated `PREFIX`/`BASE` prologue declarations before returning the operative keyword used by the read-only guards.

## Parse errors and unsupported forms

Malformed tokens, unterminated strings, trailing input, invalid numeric forms, and grammar mismatches return `QueryError::Parse`. Recognized unsupported syntax—including GQL `AS` aliases, writes, and bare SPARQL `*`—returns `QueryError::Unsupported`. GQL comma paths are parsed before their connectedness and fixed canonical shape are checked by validation/compilation. Neither parser executes SQL or mutates an input AST.
