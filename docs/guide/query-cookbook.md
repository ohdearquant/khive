# Query: Question Classes and Idiom Cookbook

The `query` verb compiles GQL (Graph Query Language) and SPARQL strings to SQL
`SELECT` statements over the knowledge graph ([ADR-008](../adr/ADR-008-query-layer-separation.md)).
It is a structural pattern matcher, not a full-text or semantic search engine,
and it is read-only: mutations always go through `create`, `update`, `link`,
`merge`, and `delete`.

This guide has two parts: a catalogue of question classes mapped to the right
verb (query, or something else), and a cookbook of verified idioms with
copy-paste syntax and known gaps.

## Routing: query vs. search vs. neighbors vs. traverse vs. context

`query` is one of five ways to read the graph. Reach for it only when the
question is genuinely structural — a typed relation pattern, a property
filter, or a bounded multi-hop path with a known relation. For everything
else, a different verb is a better fit:

| You want to...                                                                              | Use                                                        |
| ------------------------------------------------------------------------------------------- | ---------------------------------------------------------- |
| Find records by topic or keyword, fuzzy or full-text                                        | `search(kind="entity"\|"note", query="...")`               |
| Look up a known record by id                                                                | `get(id="...")`                                            |
| See immediate connections of a known node                                                   | `neighbors(node_id="...", direction="both")`               |
| Explore unbounded or loosely-bounded multi-hop paths                                        | `traverse(roots=["..."], max_depth=N)`                     |
| Pull a budgeted, entity-anchored context bundle                                             | `context(query="..."\|entity_ids=[...], hops=N, budget=N)` |
| Match a typed relation pattern, filter by property, or walk a _fixed-relation_ bounded path | `query(query="MATCH ...")` or `query(query="SELECT ...")`  |
| Create, update, link, merge, or delete anything                                             | The corresponding KG verb — never `query`                  |

Rule of thumb: if the answer depends on _relevance ranking_ or you don't know
the exact relation name, use `search`. If you already hold an id and want its
neighborhood, use `neighbors` or `traverse`. Reach for `query` only once you
can name the relation(s) and shape of the pattern you're matching.

## Question-class catalogue

Verdicts derived from the shipped parsers, AST validation, SQL compiler, the
KG verb handlers, [ADR-008](../adr/ADR-008-query-layer-separation.md), and
live read-only checks against a production knowledge graph.

| Question class            | Example question                                                    | Verdict                      | Idiom or boundary                                                                                                                                                |
| ------------------------- | ------------------------------------------------------------------- | ---------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Exact named-record lookup | Does this canonical name already exist?                             | YES via query                | `MATCH (n) WHERE n.name = "<exact name>" RETURN n.id, n.kind, n.name LIMIT 2`                                                                                    |
| Fuzzy discovery           | What records concern recursive CTE traversal?                       | YES via `search`             | `query` has no full-text or semantic operator; use `search(kind="entity", query="...")`.                                                                         |
| Immediate adjacency       | What directly annotates this known record?                          | YES via `neighbors`          | `neighbors(node_id="<uuid>", direction="incoming", relations=["annotates"])`                                                                                     |
| Targeted relation join    | Which concepts were introduced by which documents?                  | YES via query                | `MATCH (c:concept)-[e:introduced_by]->(d:document) RETURN c.name, d.name, e.weight LIMIT 50`                                                                     |
| Provenance path           | What is the derivation path from an artifact to its source?         | PARTIAL                      | Fixed `derived_from` chains, or a single variable-length edge, both work. A chain that mixes fixed and variable-length hops must be split into separate queries. |
| Supersession view         | What replaces this concept?                                         | YES via query                | `MATCH (old)-[e:supersedes]->(new) RETURN old.name, new.name, e.weight LIMIT 100`                                                                                |
| Relation alternatives     | Show records that extend or implement another record.               | YES via query                | `MATCH (a)-[e:extends\|implements]->(b) RETURN a.name, e.relation, b.name LIMIT 100`                                                                             |
| Bounded lineage           | What is reachable by one to three `extends` hops?                   | YES via query                | `MATCH (a)-[:extends*1..3]->(b) RETURN a.name, b.name LIMIT 100`                                                                                                 |
| General expansion         | Expand this known entity two hops with context.                     | YES via `traverse`/`context` | `traverse(roots=["<uuid>"], max_depth=2)`, or `context(entity_ids=["<uuid>"], hops=2, budget=N)`.                                                                |
| Cross-substrate join      | Which notes annotate concepts, documents, or other notes?           | YES via query                | `MATCH (note:note)-[e:annotates]->(target) RETURN note.name, target.kind, target.name LIMIT 100`                                                                 |
| Kind/type filtering       | Which records use a given entity type?                              | YES via query                | `MATCH (n) WHERE n.entity_type IS NOT NULL RETURN n.entity_type, n.name LIMIT 100`                                                                               |
| Property filtering        | Which names contain a token, or belong to a set of kinds?           | YES via query                | `WHERE n.name CONTAINS "<text>"`, `STARTS WITH`, or `n.kind IN ["concept", "project"]`.                                                                          |
| Edge-weight filter        | Which `extends` edges have weight at least 0.8?                     | YES via query                | `MATCH (a)-[e:extends]->(b) WHERE e.weight >= 0.8 RETURN a.name, e.weight, b.name LIMIT 100`                                                                     |
| Deduplication candidates  | Which pairs share a name or a similar description?                  | PARTIAL                      | A literal-name lookup works. Variable-to-variable comparison, grouping, and similarity scoring do not — use `search` to surface candidates instead.              |
| Orphan detection          | Which records have no graph edges?                                  | NO                           | No negative pattern, `NOT EXISTS`, optional match, or anti-join exists. Approximate outside `query` with `list` plus `neighbors`.                                |
| Edge-density audit        | Which entities have fewer than four edges?                          | PARTIAL                      | `stats()` gives a global `edges_by_relation` breakdown and `query` lists rows, but `COUNT`, `GROUP BY`, `HAVING`, and degree projection are all absent.          |
| Aggregate inventory       | How many concepts exist, by type?                                   | PARTIAL                      | Filtered row listing works. Aggregate functions, grouping, distinct projection, ordering, and offset are absent.                                                 |
| Cycle / self-reachability | Which entities are in an `extends` cycle?                           | NO                           | Repeated node-variable bindings are rejected, so cycles and self-reachability cannot be expressed.                                                               |
| Branching join            | Which parent has both an `extends` child and an `implements` child? | NO                           | GQL supports one alternating path; SPARQL rejects branched or disconnected `WHERE` blocks.                                                                       |
| Cross-namespace audit     | What belongs to another namespace?                                  | NO (caller-controlled scope) | Namespace is a runtime `CompileOptions` input, not query text — a query that references `namespace` in its body is rejected.                                     |
| Mutation via query        | Create, update, link, merge, or delete through GQL/SPARQL.          | NO (by design)               | Write-shaped input is rejected outright. Use the corresponding KG verb.                                                                                          |

## Operational limits

- GQL supports either a chain of fixed-relation hops or exactly one
  variable-length edge per query — never both mixed in the same pattern.
  Minimum hop count is 1; maximum is 10. A bare `*` (no bounds) means 1
  through 5 hops, not zero-or-more.
- A query that mixes a fixed hop with a variable-length hop in the same chain
  is rejected outright, not silently approximated. Split it into two queries.
- The server enforces an outer row cap independent of the language `LIMIT`.
  Requesting more rows than the cap allows returns a warning and truncated
  results; page with `LIMIT`/`OFFSET` — though `OFFSET` is not yet part of
  the GQL grammar, so paging past the cap currently has no in-language
  workaround.
- Result columns are projection-derived and variable-prefixed (for example
  `a_name`, `e_relation`). Returning a whole node variable expands it into
  its supported substrate columns. Variable-length paths add `_depth` and
  `_total_weight` (a running sum of edge weights along the path, not an
  average).
- SPARQL variables project as whole nodes (full column expansion), unlike
  GQL's dotted per-field projection.

## Verified GQL idioms

Each row was issued as `query(query="...")` through the `request` tool
against a production knowledge graph. Result shapes are column names and row
counts only — no graph content is reproduced here.

| #  | What it answers                       | Query string                                                                             | Result shape / pitfall                                                                                                                                                 |
| -- | ------------------------------------- | ---------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1  | Sample concept records                | `MATCH (n:concept) RETURN n LIMIT 2`                                                     | 2 rows; columns `n_created_at, n_entity_type, n_id, n_kind, n_name, n_namespace, n_properties, n_updated_at`. A whole-node variable expands to every supported column. |
| 2  | Provenance endpoints                  | `MATCH (a)-[e:introduced_by]->(b) RETURN a.name, e.relation, b.name LIMIT 2`             | 2 rows; `a_name, b_name, e_relation`.                                                                                                                                  |
| 3  | Either `extends` or `implements` edge | `MATCH (a)-[e:extends\|implements]->(b) RETURN a.name, e.relation, b.name LIMIT 2`       | 2 rows; `a_name, b_name, e_relation`. Alternation uses a pipe inside one edge pattern.                                                                                 |
| 4  | Incoming `extends` edge               | `MATCH (a)<-[e:extends]-(b) RETURN a.name, e.weight, b.name LIMIT 2`                     | 2 rows; `a_name, b_name, e_weight`.                                                                                                                                    |
| 5  | Undirected `extends` adjacency        | `MATCH (a)-[e:extends]-(b) RETURN a.name, e.relation, b.name LIMIT 2`                    | 2 rows; `a_name, b_name, e_relation`. May expose both orientations.                                                                                                    |
| 6  | One- to two-hop lineage               | `MATCH (a)-[e:extends*1..2]->(b) RETURN a.name, b.name LIMIT 2`                          | 2 rows; `_depth, _total_weight, a_name, b_name`. Bounds are inclusive; maximum depth is 10.                                                                            |
| 7  | Default variable-length lineage       | `MATCH (a)-[e:extends*]->(b) RETURN a.name, b.name LIMIT 2`                              | 2 rows; `_depth, _total_weight, a_name, b_name`. Bare `*` means 1 through 5 hops, not zero-or-more.                                                                    |
| 8  | Weight threshold                      | `MATCH (a)-[e:extends]->(b) WHERE e.weight >= 0 RETURN a.name, e.weight, b.name LIMIT 2` | 2 rows; `a_name, b_name, e_weight`. Edge predicates support `relation` and `weight`.                                                                                   |
| 9  | Kind equality filter                  | `MATCH (n) WHERE n.kind = "concept" RETURN n.name, n.kind LIMIT 2`                       | 2 rows; `n_kind, n_name`. Escape inner literal quotes in the outer DSL string.                                                                                         |
| 10 | Case-insensitive substring            | `MATCH (n) WHERE n.name CONTAINS "ADR" RETURN n.name LIMIT 2`                            | 2 rows; `n_name`. `CONTAINS` escapes literal `%`, `_`, and `\`.                                                                                                        |
| 11 | Prefix lookup                         | `MATCH (n) WHERE n.name STARTS WITH "ADR" RETURN n.name LIMIT 2`                         | 2 rows; `n_name`.                                                                                                                                                      |
| 12 | Multi-value filter                    | `MATCH (n) WHERE n.kind IN ["concept", "project"] RETURN n.name, n.kind LIMIT 2`         | Live-verified: 2 rows; `n_kind, n_name`. `IN` requires a scalar list; `null` is illegal.                                                                               |
| 13 | Indexed entity-type filter            | `MATCH (n) WHERE n.entity_type IS NOT NULL RETURN n.entity_type LIMIT 2`                 | 2 rows; `n_entity_type`. `IS NOT NULL` takes no right-hand literal.                                                                                                    |
| 14 | `WHERE ... OR ...`                    | `MATCH (n) WHERE n.kind = "concept" OR n.kind = "project" RETURN n.name, n.kind LIMIT 2` | 2 rows; `n_kind, n_name`. `AND` binds tighter than `OR`; parentheses are not available to force grouping.                                                              |
| 15 | Outer cap vs. language `LIMIT`        | `query(query="MATCH (n) RETURN n.name LIMIT 5", limit=2)`                                | 2 rows; warning: result set capped at 2 rows; requested limit 5 exceeds the cap. The outer `limit` argument is a hard server cap independent of the language `LIMIT`.  |

## Rejected forms (live-checked)

Each is a verified read-only check against the running server. Errors are
verbatim.

| Form                           | Query string                                                      | Observed error                                                                                                                                               |
| ------------------------------ | ----------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| GQL write                      | `CREATE (n:concept)`                                              | `query: unsupported feature: the query verb is read-only; to mutate the graph use: create, update, link, merge, delete`                                      |
| SPARQL update                  | `INSERT DATA { ?a :extends ?b }`                                  | same read-only rejection as above                                                                                                                            |
| Mixed fixed and variable chain | `MATCH (a)-[e:extends]->(b)-[f:extends*1..2]->(c) RETURN a, b, c` | `query: unsupported feature: variable-length patterns must be a single start_node -[*N..M]-> end_node (mixed fixed/variable chains are not yet implemented)` |
| Zero-hop GQL range             | `MATCH (a)-[e:extends*0..2]->(b) RETURN a, b`                     | `query: unsupported feature: zero-hop ranges (min_hops = 0) not yet supported; use a minimum of 1 hop`                                                       |
| Unknown relation               | `MATCH (a)-[e:bogus]->(b) RETURN a, b`                            | `query: validation error: unknown edge_relation: "bogus"` (lists the 17 valid relations)                                                                     |
| Repeated binding / cycle       | `MATCH (a)-[e:extends]->(a) RETURN a`                             | `query: unsupported feature: repeated node variable 'a' (cycle / self-reachability requires alias-equality predicates not yet implemented)`                  |
| Branched SPARQL                | `SELECT ?a ?b ?c WHERE { ?a :extends ?b . ?a :implements ?c . }`  | `query: unsupported feature: SPARQL WHERE block is branched or disconnected; only single-path patterns are supported`                                        |
| SPARQL zero-or-more            | `SELECT ?a WHERE { ?a :extends* ?b . }`                           | `query: unsupported feature: SPARQL '*' (zero-or-more hops) not yet supported; use '+' or '{min,max}'`                                                       |
| Namespace from query text      | `MATCH (n) WHERE n.namespace = "local" RETURN n LIMIT 1`          | `query: validation error: namespace is set by CompileOptions, not query text`                                                                                |
| Incomplete float literal       | `MATCH (n) WHERE n.name = 1. RETURN n LIMIT 1`                    | `query: parse error: float literal must have digits after '.' (e.g. '1.0', not '1.')`                                                                        |

## SPARQL

SPARQL positive-path execution is verified end to end: a one-hop `SELECT`
returns rows on the live MCP tool.

| What it answers                                     | Query string                                                            | Notes                                                                                                                                                |
| --------------------------------------------------- | ----------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
| Which endpoints have an `extends` edge              | `SELECT ?a ?b WHERE { ?a :extends ?b . } LIMIT 2`                       | 2 rows. SPARQL variables project as whole nodes — full column expansion (`a_id, a_kind, a_name, a_properties, ...`), unlike GQL's dotted projection. |
| Which concepts have an `introduced_by` source       | `SELECT ?a ?b WHERE { ?a a :concept . ?a :introduced_by ?b . } LIMIT 2` | Typed-node triple pattern (`?a a :concept`) combined with a relation triple.                                                                         |
| Which endpoints are one or two `extends` hops apart | `SELECT ?a ?b WHERE { ?a :extends{1,2} ?b . } LIMIT 2`                  | Bounded variable-length path using the `{min,max}` form.                                                                                             |

## Gaps

- Mixed fixed-plus-variable-length paths are rejected; split the query into
  separate calls instead.
- No anti-join, optional match, aggregate, `GROUP BY`, `DISTINCT`, ordering,
  or `OFFSET` exists. Orphan-detection and edge-density questions cannot be
  expressed in a single query.
- A repeated node-variable binding is rejected, which blocks cycle and
  self-reachability queries.
- SPARQL is single-path only; a branched `WHERE` block is rejected.
- The row-cap warning recommends `LIMIT`/`OFFSET` paging, but `OFFSET` is not
  yet part of the GQL grammar.

## See also

- [ADR-008: Query Layer Separation](../adr/ADR-008-query-layer-separation.md) — why GQL and SPARQL share one SQL compilation target and no mutation path.
- [Search and Retrieval](search.md) — fuzzy, full-text, and hybrid retrieval when `query` isn't the right tool.
- [Prompt Cookbook](prompt-cookbook.md) — verb patterns beyond `query`.
- [AGENTS.md](../../AGENTS.md) — additional GQL and SPARQL examples for agents.
