# Query: Question Classes and Idiom Cookbook

The `query` verb compiles GQL (Graph Query Language) and SPARQL strings to SQL
`SELECT` statements over the knowledge graph ([ADR-008](../adr/ADR-008-query-layer-separation.md)).
It is a structural pattern matcher, not a full-text or semantic search engine,
and it is read-only: mutations always go through `create`, `update`, `link`,
`merge`, and `delete`.

This guide has two parts: a catalogue of question classes mapped to the right
verb (query, or something else), and a cookbook of supported idioms with
copy-paste syntax and known gaps.

## Routing: query vs. search vs. neighbors vs. traverse vs. context

`query` is one of five ways to read the graph. Reach for it only when the
question is genuinely structural — a typed relation pattern, a property
filter, or a bounded multi-hop path with a known relation. For everything
else, a different verb is a better fit:

| You want to...                                                                              | Use                                                                                             |
| ------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| Find records by topic or keyword, fuzzy or full-text                                        | `search(kind="entity", query="...")` — `kind="note"` works the same way                         |
| Look up a known record by id                                                                | `get(id="...")`                                                                                 |
| See immediate connections of a known node                                                   | `neighbors(node_id="...", direction="both")`                                                    |
| Explore bounded multi-hop paths                                                             | `traverse(roots=["..."], max_depth=N)` — depth ≤10 with finite result/work/time budgets         |
| Pull a budgeted, entity-anchored context bundle                                             | `context(query="...", hops=N, budget=N)` — or anchor with `entity_ids=[...]` instead of `query` |
| Match a typed relation pattern, filter by property, or walk a _fixed-relation_ bounded path | `query(query="MATCH ...")` or `query(query="SELECT ...")`                                       |
| Create, update, link, merge, or delete anything                                             | The corresponding KG verb — never `query`                                                       |

Rule of thumb: if the answer depends on _relevance ranking_ or you don't know
the exact relation name, use `search`. If you already hold an id and want its
neighborhood, use `neighbors` or `traverse`. Reach for `query` only once you
can name the relation(s) and shape of the pattern you're matching.

## Question-class catalogue

Verdicts derived from the shipped parsers, AST validation, SQL compiler, the
KG verb handlers, and [ADR-008](../adr/ADR-008-query-layer-separation.md).
Historical live observations are recorded in the Verified GQL idioms and
Rejected forms sections below.

| Question class            | Example question                                                    | Verdict                      | Idiom or boundary                                                                                                                                                                                                                                                                                                                                               |
| ------------------------- | ------------------------------------------------------------------- | ---------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Exact named-record lookup | Does this canonical name already exist?                             | YES via query                | `MATCH (n) WHERE n.name = "<exact name>" RETURN n.id, n.kind, n.name LIMIT 2`                                                                                                                                                                                                                                                                                   |
| Fuzzy discovery           | What records concern recursive CTE traversal?                       | YES via `search`             | `query` has no full-text or semantic operator; use `search(kind="entity", query="...")`.                                                                                                                                                                                                                                                                        |
| Immediate adjacency       | What directly annotates this known record?                          | YES via `neighbors`          | `neighbors(node_id="<uuid>", direction="incoming", relations=["annotates"])`                                                                                                                                                                                                                                                                                    |
| Targeted relation join    | Which concepts were introduced by which documents?                  | YES via query                | `MATCH (c:concept)-[e:introduced_by]->(d:document) RETURN c.name, d.name, e.weight LIMIT 50`                                                                                                                                                                                                                                                                    |
| Provenance path           | What is the derivation path from an artifact to its source?         | YES via query                | Canonical paths may combine fixed steps and bounded variable segments; their variable maxima sum to at most ten. See mixed canonical paths below.                                                                                                                                                                                                               |
| Supersession view         | What replaces this concept?                                         | YES via query                | `MATCH (old)-[e:supersedes]->(new) RETURN old.name, new.name, e.weight LIMIT 100`                                                                                                                                                                                                                                                                               |
| Relation alternatives     | Show records that extend or implement another record.               | YES via query                | Pipe-separated relation alternation inside one edge pattern — see the fenced example below the table (a literal pipe cannot be shown inside this table cell).                                                                                                                                                                                                   |
| Bounded lineage           | What is reachable by one to three `extends` hops?                   | YES via query                | `MATCH (a)-[:extends*1..3]->(b) RETURN a.name, b.name LIMIT 100`                                                                                                                                                                                                                                                                                                |
| General expansion         | Expand this known entity two hops with context.                     | YES via `traverse`/`context` | `traverse(roots=["<uuid>"], max_depth=2)`, or `context(entity_ids=["<uuid>"], hops=2, budget=N)`.                                                                                                                                                                                                                                                               |
| Cross-substrate join      | Which notes annotate concepts, documents, or other notes?           | YES via query                | `MATCH (note:note)-[e:annotates]->(target) RETURN note.name, target.kind, target.name LIMIT 100`                                                                                                                                                                                                                                                                |
| Pack-note inventory       | Which caller-visible comm messages exist?                           | YES via query                | `MATCH (m:message) RETURN m.id, m.kind LIMIT 100`; equivalently, use `MATCH (m:note) WHERE m.kind = "message" RETURN m.id, m.kind LIMIT 100`.                                                                                                                                                                                                                   |
| Kind/type filtering       | Which records use a given entity type?                              | YES via query                | `MATCH (n) WHERE n.entity_type IS NOT NULL RETURN n.entity_type, n.name LIMIT 100`                                                                                                                                                                                                                                                                              |
| Property filtering        | Which names contain a token, or belong to a set of kinds?           | YES via query                | `WHERE n.name CONTAINS "<text>"`, `STARTS WITH`, or `n.kind IN ["concept", "project"]`.                                                                                                                                                                                                                                                                         |
| Edge-weight filter        | Which `extends` edges have weight at least 0.8?                     | YES via query                | `MATCH (a)-[e:extends]->(b) WHERE e.weight >= 0.8 RETURN a.name, e.weight, b.name LIMIT 100`                                                                                                                                                                                                                                                                    |
| Deduplication candidates  | Which pairs share a name or a similar description?                  | PARTIAL                      | Literal-name lookup and returned-value grouping with count work. Variable-to-variable comparison and similarity scoring remain unsupported; use `search` to surface candidates.                                                                                                                                                                                 |
| Orphan detection          | Which records have no visible canonical graph edges?                | YES                          | Correlated `NOT EXISTS { (n)-[]-(other) }` selects records without an accessible canonical edge. Inner paths may have several fixed steps; hidden or deleted support does not suppress a result.                                                                                                                                                                |
| Edge-density audit        | Which entities have fewer than four edges?                          | PARTIAL                      | `stats()` gives global `edges_by_relation` and `edges_by_endpoint_base` breakdowns and `query` can group matched edge rows with `count(e)`. Explicit `GROUP BY`, `HAVING`, zero-degree outer matches and degree predicates remain unsupported. For a graph-wide density figure use `edges_structural`, not `edges`: the flat total counts provenance edges too. |
| Aggregate inventory       | How many concepts exist, by type?                                   | YES via query                | Filtered row listing, `ORDER BY`, and GQL `SKIP` paging work. Use `MATCH (n:concept) RETURN n.entity_type, count(n)` for grouped inventory; explicit DISTINCT is also supported.                                                                                                                                                                                |
| Cycle / self-reachability | Which entities are in an `extends` cycle?                           | PARTIAL                      | Fixed canonical cycles can join shared nodes across comma paths. A repeated node within one path and recursive multi-pattern cycles remain unsupported.                                                                                                                                                                                                         |
| Branching join            | Which parent has both an `extends` child and an `implements` child? | YES via GQL                  | Connected comma paths share named nodes and retain tuple multiplicity; see the star recipe below. SPARQL still rejects branching.                                                                                                                                                                                                                               |
| Cross-namespace audit     | What belongs to another namespace?                                  | NO (caller-controlled scope) | Namespace visibility is supplied by runtime `CompileOptions`. Direct namespace predicates and order fields are rejected; returned namespace fields and a metadata key named namespace do not override scope.                                                                                                                                                    |
| Mutation via query        | Create, update, link, merge, or delete through GQL/SPARQL.          | NO (by design)               | Write-shaped input is rejected outright. Use the corresponding KG verb.                                                                                                                                                                                                                                                                                         |

The relation-alternatives idiom, copy-paste form (alternation is a bare pipe
between relation identifiers inside one edge pattern):

```
MATCH (a)-[e:extends|implements]->(b) RETURN a.name, e.relation, b.name LIMIT 100
```

## Operational limits

- GQL accepts connected comma-separated paths after `MATCH`, with one global
  WHERE, RETURN, ORDER BY, SKIP and LIMIT tail. Every edge in a multi-pattern
  query must be canonical and fixed at one hop; a component can contain several
  such edges. Shared named nodes connect components; anonymous nodes do not.
  SPARQL remains single-path only.
- `RETURN` accepts comma-separated variables, `variable.property` items, and
  named paths below node `properties` or canonical edge `metadata`, and
  `count(binding)`. `RETURN DISTINCT` deduplicates returned values; `AS` aliases remain unsupported. Output names remain variable-prefixed, such as `a_id`.
- A single canonical GQL path supports fixed steps and multiple bounded variable
  segments. Their maximum hop counts sum to at most 10; fixed steps do not count
  toward that sum. Explicit ranges may start at zero. A bare `*` means 1 through
  5 hops; `*1` and `*1..1` are fixed steps.
- The query verb's `page_size` has minimum 1, defaults to 500, and is clamped
  to a hard cap of 10,000. GQL `SKIP` advances through deterministically ordered matches. Omit
  query-text `LIMIT` for an exhaustive audit; a `LIMIT` at or below `page_size`
  is a terminal caller-chosen bound. The legacy outer `limit` argument remains
  a deprecated, mutually exclusive alias for `page_size`.
- Result columns are projection-derived and variable-prefixed (for example
  `a_name`, `e_relation`). Returning a whole node variable expands it into
  its supported substrate columns. Ordinary variable-length projections add `_depth` and
  `_total_weight` (a running sum of edge weights along the path, not an
  average). Explicit DISTINCT and grouped counts omit these implicit metrics.
- Pack-registered note kinds use the same GQL `kind` field as base kinds. A granular label
  such as `:message` is shorthand for the stored kind; `:note` selects the note substrate and
  can be combined with `WHERE m.kind = "message"`. `note_kind` belongs to the verb parameter
  surface and is not a GQL field. Both forms remain restricted to the caller's authorized
  namespace scopes.
- SPARQL variables project as whole nodes (full column expansion), unlike
  GQL's dotted per-field projection. SPARQL `OFFSET` is not supported.

## Requirements without visible support

```text
MATCH (r:concept {role: 'requirement'})
WHERE NOT EXISTS { (f:concept)-[:supports]->(r) }
RETURN r.name ORDER BY r.name
```

The existing outer `r` is correlated by substrate and UUID. The inner `f` is local to this block. Visible support suppresses the requirement; unrelated, hidden, deleted or out-of-scope support does not. Multiple support paths do not duplicate the outer row. To find records with no visible canonical incident edge, use `MATCH (n) WHERE NOT EXISTS { (n)-[]-(other) } RETURN n.id`.

Each block contains one fixed canonical path and at least one outer node name. It may have several steps with ordinary node constraints, or just one correlated node. Local names may be reused in separate blocks but cannot appear in outer RETURN, count, ORDER or scalar predicates. Outer edge names cannot be reused as inner edge names; edge variables are not correlated into NOT EXISTS. Repeated names within a path and node/edge conflicts remain refused.

Absence atoms compose with scalar predicates under AND-before-OR precedence. A recursive outer query can correlate its start, end, or both. Start-only blocks run at the seed; blocks using the end run at the final row. Cross-phase OR remains unsupported. Filtering happens before counts, DISTINCT and paging, so counts cover all surviving matches. This adds no OPTIONAL MATCH, inner query clauses, comma/nested blocks, recursive inner paths or synthetic inner edges.

## Paging broad GQL results

The response always includes `offset`, effective `page_size`, `has_more`, and
the compatibility alias `truncated`. A non-terminal GQL page also includes
`next_offset`; set the next query's `SKIP` to that exact value and keep the same
`page_size`:

```text
query(query="MATCH (a)-[r:depends_on]->(b) RETURN a.id, r.id, b.id", page_size=500)
# ... "has_more": true, "next_offset": 500

query(query="MATCH (a)-[r:depends_on]->(b) RETURN a.id, r.id, b.id SKIP 500", page_size=500)
```

`next_offset` advances by the number of rows actually emitted. It is omitted
when `has_more` is false. After requested ORDER terms, ordinary fixed results use
complete bound identities (including synthetic observation primary keys); ordinary
recursive results use depth, weight and projected values. Explicit DISTINCT and
grouped results use only returned values as ties. Repeated reads
of unchanged data are therefore stable. This is offset paging, not a cross-call database
snapshot: inserts or deletes that match the query can shift later pages.

## Ordering and counts

GQL accepts `ORDER BY` between `RETURN` and `SKIP`/`LIMIT`. Terms are supported
scalar fields or named paths below node `properties` or canonical edge `metadata`;
each takes optional `ASC` (the default) or `DESC`. Ordinary queries retain identity
ties for stable continuation; explicit DISTINCT and grouped counts use only returned
values. In those modes every order term must derive from a returned value. The
following verbs remain useful for relevance-oriented or type-inventory reads:

| You want                         | Use                                                                                                                                                                                                                                                                                                                                                                                                             |
| -------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| The newest records about a topic | `search(kind="note", query="...", order_by="created_at")`, or `order_by="updated_at"`. Results sort newest first within the bounded candidate window, so this ranks recent matches but is not a full sort.                                                                                                                                                                                                      |
| The newest records of a type     | `list(kind="entity", entity_type="...", limit=1)`, or `list(kind="note", note_kind="...")`. Offset pages sort newest first by `created_at` over the whole filtered set; see [`list`](api-reference.md#list--assertive) for the tie order.                                                                                                                                                                       |
| How many entities of a type      | `stats()` returns `entities_by_type`: one row per stored `entity_type` across entity kinds, counted over caller-visible namespaces and live rows (its `count_scope`). The first row counts entities with no type; in Agent presentation it carries only `count`, because null fields are dropped. For a count under other filters, such as kind or tags, page `list(..., after="")` until `next_after` is null. |

### Distinct values and grouped counts

```text
MATCH (n:concept) RETURN DISTINCT n.entity_type
MATCH (n:concept) RETURN n.entity_type, count(n) ORDER BY n.entity_type
MATCH (a {name: "root"})-[e:extends*0..2]->(b) RETURN count(b), count(e)
```

The first query returns one row per value; the second groups by returned type and
names the integer count `count_n`. Counts-only empty input yields one zero row;
grouped empty input yields none. Missing and JSON-null values share a null group.
Text grouping is case-sensitive, while numbers retain numeric comparison.

Counts measure matching bindings: repeated node or edge occurrences on different
paths count separately. A zero-hop node contributes one and its absent final edge
contributes zero. Synthetic observation edges count their matching rows even when
the displayed edge ID is null. With counts, every other returned field is a group
key; returning a whole binding groups by all its expanded fields. Neither grouping
nor explicit DISTINCT adds hidden IDs, `_depth` or `_total_weight` to the result.

Complete matching and filtering precede grouping and DISTINCT; ordering, SKIP and
LIMIT/page-size continuation follow them. Each page retains complete group counts.
A count operand alone does not expose that binding's fields for ORDER BY. The
supported aggregate is `count(binding)` only: no `count(*)`, `count(DISTINCT ...)`,
property operands, `AS`, explicit GROUP BY, HAVING or count/alias ORDER syntax.

### Ordered qualified relations

To retrieve canonical relations with their qualification data in order:

```text
MATCH (a)-[e:depends_on]->(b)
WHERE e.metadata.order IS NOT NULL
RETURN a.id, e.id, e.metadata, b.id
ORDER BY e.metadata.order ASC, e.weight DESC
LIMIT 100
```

`e_metadata` contains the stored JSON text, or SQL null when metadata is absent.
Returning only `e` preserves the existing edge fields without metadata: five for
this fixed-length pattern, or final-edge id/relation/weight for a recursive pattern.
Metadata paths also support nested identifiers, comparisons and literal string
filters; use `e.metadata.label CONTAINS "draft_1"` to match the underscore literally.
Node metadata and edge properties are unsupported. Synthetic `observed_as_*`
edges do not carry canonical metadata.

Ordering preserves SQLite types: null sorts first ascending and last descending,
numbers sort numerically, and text sorts with BINARY collation. JSON booleans act
as integers; object/array leaves sort as their extracted JSON text. Text equality
and LIKE-family predicates remain case-insensitive, so their matching rule differs from text ordering. Bare
metadata/properties containers, namespace fields, functions, aliases and `NULLS`
clauses are not order terms.

For a variable-length path, metadata belongs to its final edge. Recursive order
terms must be derivable from the actual returned fields: `RETURN e.metadata
ORDER BY e.metadata.order` is valid, but `RETURN e ORDER BY e.metadata.order`
is rejected because whole-edge projections omit metadata. This preserves the
compiler's recursive `DISTINCT` behavior without hidden sort columns.

For multiple pages, retain the same order terms and page size and advance `SKIP`
using `next_offset`. The query-text `LIMIT` applies after `SKIP` at each continued
offset. An explicit limit at or below `page_size` produces a terminal page even
when more matches exist; omit it for an exhaustive read. Visibility filtering
precedes sorting and paging.
See [ADR-008 Amendment 1](../adr/ADR-008-query-layer-separation.md#amendment-1-edge-metadata-and-bounded-gql-ordering-2026-11-14)
for the ordering and recursive contracts.

## Return selected JSON values

Return named leaves when the full properties or metadata container is unnecessary:

```text
MATCH (a:concept)-[e:extends*0..2]->(b:concept)
RETURN b.properties.status, e.metadata.rank.order
ORDER BY e.metadata.rank.order
LIMIT 100
```

The columns are `b_properties_status` and `e_metadata_rank_order`, plus the usual
recursive depth and accumulated weight. Numbers stay numeric, strings stay text,
booleans are `0` or `1`, and a missing or JSON-null leaf is null. Object and array
leaves are JSON text. The edge value belongs to the final traversed edge and is
null at zero hops. `RETURN b.properties` and `RETURN e.metadata` still return their
stored containers; whole-node and whole-edge projections keep their existing shape.

Use explicit `properties` or `metadata` roots and named identifier segments.
Metadata requires a canonical edge. Different expressions cannot share one exact
column name: `b.properties.a_b` and `b.properties.a.b` would both be named
`b_properties_a_b`, so returning both is refused. Output labels preserve case and
are not `AS` aliases or new ordering references. On a recursive path, order by an
exactly returned path or a path within a returned whole container; a different
hidden path remains unsupported.

## Connected branches around one node

Read two kinds of neighbour around the same concept in one query:

```text
MATCH (a:concept)-[e:extends]->(b:concept),
      (a)-[f:variant_of]->(c:concept)
WHERE e.metadata.order >= 0 AND f.metadata.enabled = true
RETURN a.id, b.id, c.id, e.id, f.id, e.metadata, f.metadata
ORDER BY e.metadata.order ASC, f.metadata.order DESC
```

Both occurrences of `a` identify the same substrate and UUID. Their kind, subtype
and inline-property constraints all apply. A center with two matching `extends`
edges and three matching `variant_of` edges produces six tuples. Returning only
`a.id` still returns six rows; equal projections are not deduplicated. Different
edge variables can select the same edge when both patterns match it.

Every component must connect through shared named nodes. A later path can bridge
earlier components, and a shared singleton node path can add constraints. Repeating
a node within one path, repeating an edge variable anywhere, or using one name for
both a node and an edge remains unsupported. Recursive and synthetic observation
paths retain their single-path behavior and cannot join a comma-pattern query.

The WHERE tree and all projections and order terms can refer to any component.
Each occurrence retains namespace, deletion and mailbox filtering before paging.
For ordinary projections, requested order is followed by the full occurrence-identity
suffix in textual path order; DESC does not reverse those ties. Explicit DISTINCT
and grouped counts instead use their returned values as ties. Continue with `next_offset` as in
[Paging broad GQL results](#paging-broad-gql-results), retaining the same query
LIMIT after each new offset. An explicit limit at or below `page_size` is terminal.
Offset pages are stable only for unchanged data, and page size does not
bound the combinations matched or sorted.

For a simple inventory of edge records, one path avoids multiplying combinations:

```text
MATCH (a)-[e]->(b) RETURN a.id, e.id, e.relation, b.id
```

Group endpoint IDs client-side while retaining each edge ID and relation when an
edge inventory, rather than joined tuples, answers the question.

See [ADR-008 Amendment 2](../adr/ADR-008-query-layer-separation.md#amendment-2-connected-comma-separated-match-patterns-2026-11-14)
for the connected-pattern contract.

## Include the starting record in a bounded lineage

Use a zero minimum when the starting record itself should be an answer:

```text
MATCH (a:concept {name: "root"})-[e:extends*0..2]->(b)
RETURN a.id, b.id, e.id, e.relation, e.weight, e.metadata
```

At depth zero, `a` and `b` are the same typed record, `_depth` is `0`, and
`_total_weight` is `0.0`. All returned edge fields are null. An isolated root can
match without an incident edge. Constraints on both endpoints still apply, so
an incompatible end kind or property excludes that zero-hop row. Ordinary
namespace, deletion and mailbox restrictions apply before the row is returned.

`*0..0` returns only the starting record; `*0..1` also permits one edge. Incoming
and undirected ranges behave the same way. A positive minimum excludes the
zero-hop row. The maximum remains 10 and later revisits remain blocked; the
existing first-step self-loop behavior is retained. Bare GQL `*` still means
one through five hops. Explicit SPARQL ranges such as `:extends{0,2}` share these
zero-hop rules; bare SPARQL `*` remains unsupported.

## Verified GQL idioms

Each row was issued as `query(query="...")` through the `request` tool
against a production knowledge graph. Result shapes are column names and row
counts only — no graph content is reproduced here.

| #  | What it answers                       | Query string                                                                             | Result shape / pitfall                                                                                                                                                 |
| -- | ------------------------------------- | ---------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1  | Sample concept records                | `MATCH (n:concept) RETURN n LIMIT 2`                                                     | 2 rows; columns `n_created_at, n_entity_type, n_id, n_kind, n_name, n_namespace, n_properties, n_updated_at`. A whole-node variable expands to every supported column. |
| 2  | Provenance endpoints                  | `MATCH (a)-[e:introduced_by]->(b) RETURN a.name, e.relation, b.name LIMIT 2`             | 2 rows; `a_name, b_name, e_relation`.                                                                                                                                  |
| 3  | Either `extends` or `implements` edge | The alternation idiom in the fenced block above, with `LIMIT 2`                          | 2 rows; `a_name, b_name, e_relation`. Alternation uses a pipe inside one edge pattern.                                                                                 |
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
| 15 | Page size vs. query `LIMIT`           | `query(query="MATCH (n) RETURN n.name LIMIT 5", page_size=2)`                            | 2 rows plus `has_more: true` and `next_offset: 2`; query `LIMIT` exceeds the effective page size, so the +1 sentinel detects another page.                             |

## Dialect boundaries

Disconnected comma paths are refused during validation/compilation; return aliases
receive a specific parser error:

| Form                       | Query string                                                     | Contract and alternative                                                                                                                  |
| -------------------------- | ---------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------- |
| Disconnected comma paths   | `MATCH (a), (b) RETURN a, b`                                     | `QueryError::Unsupported` requires components connected through shared named nodes. A star with shared bindings is supported.             |
| `RETURN` aliases with `AS` | `MATCH (a)-[e]->(b) RETURN a.name AS src, b.name AS dst LIMIT 5` | `QueryError::Unsupported` names `AS`. Use unaliased `variable` or `variable.property` projections; result names remain variable-prefixed. |

## Rejected forms (live-checked)

Each is a verified read-only check against the running server. Errors are
verbatim.

| Form                      | Query string                                                     | Observed error                                                                                                                              |
| ------------------------- | ---------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| GQL write                 | `CREATE (n:concept)`                                             | `query: unsupported feature: the query verb is read-only; to mutate the graph use: create, update, link, merge, delete`                     |
| SPARQL update             | `INSERT DATA { ?a :extends ?b }`                                 | same read-only rejection as above                                                                                                           |
| Unknown relation          | `MATCH (a)-[e:bogus]->(b) RETURN a, b`                           | `query: validation error: unknown edge_relation: "bogus"` (lists the 17 valid relations)                                                    |
| Repeated binding / cycle  | `MATCH (a)-[e:extends]->(a) RETURN a`                            | `query: unsupported feature: repeated node variable 'a' (cycle / self-reachability requires alias-equality predicates not yet implemented)` |
| Branched SPARQL           | `SELECT ?a ?b ?c WHERE { ?a :extends ?b . ?a :implements ?c . }` | `query: unsupported feature: SPARQL WHERE block is branched or disconnected; only single-path patterns are supported`                       |
| SPARQL zero-or-more       | `SELECT ?a WHERE { ?a :extends* ?b . }`                          | `query: unsupported feature: SPARQL '*' (zero-or-more hops) not yet supported; use '+' or '{min,max}'`                                      |
| Namespace from query text | `MATCH (n) WHERE n.namespace = "local" RETURN n LIMIT 1`         | `query: validation error: namespace is set by CompileOptions, not query text`                                                               |
| Incomplete float literal  | `MATCH (n) WHERE n.name = 1. RETURN n LIMIT 1`                   | `query: parse error: float literal must have digits after '.' (e.g. '1.0', not '1.')`                                                       |

## SPARQL

SPARQL positive-path execution is verified end to end: a one-hop `SELECT`
returns rows on the live MCP tool.

| What it answers                                     | Query string                                                            | Notes                                                                                                                                                |
| --------------------------------------------------- | ----------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
| Which endpoints have an `extends` edge              | `SELECT ?a ?b WHERE { ?a :extends ?b . } LIMIT 2`                       | 2 rows. SPARQL variables project as whole nodes — full column expansion (`a_id, a_kind, a_name, a_properties, ...`), unlike GQL's dotted projection. |
| Which concepts have an `introduced_by` source       | `SELECT ?a ?b WHERE { ?a a :concept . ?a :introduced_by ?b . } LIMIT 2` | Typed-node triple pattern (`?a a :concept`) combined with a relation triple.                                                                         |
| Which endpoints are one or two `extends` hops apart | `SELECT ?a ?b WHERE { ?a :extends{1,2} ?b . } LIMIT 2`                  | Bounded variable-length path using the `{min,max}` form.                                                                                             |

## Mixed canonical paths

These examples describe the new contract; they have not been live-checked. A path
with variable maxima 4 and 7 exceeds the cumulative cap and returns `InvalidInput`.

```text
MATCH (root {name: "root"})-[lineage:extends*0..3]->(descendant)-[attachment:contains]->(document)
RETURN descendant.name, document.name, lineage.metadata.rank, attachment.id
ORDER BY descendant.name, document.name
LIMIT 100
```

The zero-hop match can return the root's own attachments; its `lineage` edge fields
are null. Each variable segment keeps its own final edge. Ordinary `_depth` and
`_total_weight` sum variable hops and weights only, excluding fixed steps. Several
variable segments are allowed when their maximum hop counts sum to at most ten;
`*1` and `*1..1` are fixed steps. Cycle tracking is local to each variable segment.

Mixed WHERE expressions, including OR across fixed and variable bindings and
absence correlated to intermediate nodes, filter complete matches before counts,
DISTINCT or paging. The legacy single-variable-segment cross-phase OR refusal
remains. Output limits do not cap intermediate fanout.

## Gaps

- Disconnected, recursive or synthetic multi-pattern GQL matches remain
  unsupported. Connected canonical paths with fixed one-hop edges are supported.
  `RETURN ... AS ...` aliases still return a named unsupported-feature error;
  use unaliased projections.
- Recursive comma patterns and synthetic steps within mixed variable paths remain unsupported.
- Correlated canonical fixed-path `NOT EXISTS` supports absence and orphan detection.
  Optional matches, explicit `GROUP BY`, `HAVING` and aggregates other than
  `count(binding)` remain unsupported. GQL supports RETURN DISTINCT and
  implicit grouping, and uses `SKIP` rather than `OFFSET`; SPARQL OFFSET remains
  unsupported. Zero-degree count groups still need other reads or separate absence queries.
  See [ordering and counts](#ordering-and-counts).
- A repeated node-variable binding within one path remains rejected. Fixed
  cycles across separate connected canonical paths can reuse node bindings;
  this does not add general recursive cycle or self-reachability syntax.
- SPARQL is single-path only; a branched `WHERE` block is rejected.

## See also

- [GQL and SPARQL parsing](../../crates/khive-query/docs/api/parsing.md) — the concise grammar and unsupported-form contract.
- [ADR-008: Query Layer Separation](../adr/ADR-008-query-layer-separation.md) — why GQL and SPARQL share one SQL compilation target and no mutation path.
- [Search and Retrieval](search.md) — fuzzy, full-text, and hybrid retrieval when `query` isn't the right tool.
- [Prompt Cookbook](prompt-cookbook.md) — verb patterns beyond `query`.
- [AGENTS.md](../../AGENTS.md) — additional GQL and SPARQL examples for agents.
