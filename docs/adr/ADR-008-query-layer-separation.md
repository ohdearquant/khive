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
   SQLite, no ATTACH awareness, and no backend topology knowledge.
3. **Relation validation.** The canonical edge relations defined by ADR-002 are validated by
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

Granular node kinds follow the same read-time boundary. `khive-query` does not receive the
`VerbRegistry` and must not copy its pack-owned note-kind or entity-kind vocabulary. A granular
label such as `MATCH (m:message)` is compiled as a bound comparison against the shared `kind`
column; a substrate label such as `note` is compiled against the union's `substrate_kind`
discriminator. Consequently these GQL forms are equivalent for caller-visible comm rows:

```text
MATCH (m:message) RETURN m.id
MATCH (m:note) WHERE m.kind = "message" RETURN m.id
```

Registration remains authoritative on the write path: comm registers `message`, the runtime
admits message notes into the shared `notes` table, and the query compiler can then match the
stored value without a registry dependency. `note_kind` is a verb-parameter name, not a GQL
field; GQL uses `kind`. Namespace visibility is still injected by `CompileOptions`, so a query
does not reveal message copies outside the caller's authorized scopes.

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

### Bounded GQL result pages

GQL accepts `SKIP <offset>` before an optional `LIMIT <rows>`. The shared AST stores the
offset, and the SQL compiler applies it only after a deterministic total order over bound match
identities. Fixed canonical patterns order by substrate discriminators and UUIDs for every node
and edge; synthetic observation edges use their `(event_id, role, position)` primary key.
Recursive patterns use `DISTINCT`, so they order by depth, weight, and every projected output
alias—a total tuple over the rows that survive projection. This makes pages repeatable while the
matched graph is unchanged without exposing caller-defined ordering in the query dialect.

See [Amendment 1][A1] for the subsequent caller-defined ordering contract.

The query verb supplies a bounded `page_size` (minimum 1, default 500, hard cap 10,000). Query-text
`LIMIT` and `page_size` compose as the smaller payload bound. When the server page size binds,
compilation fetches one sentinel row; runtime removes it and returns `has_more`, compatibility
`truncated`, and a GQL `next_offset` equal to the requested offset plus emitted rows. An explicit
query `LIMIT` at or below the page size remains a terminal caller-selected bound.

SPARQL `OFFSET` is intentionally not added by this decision. Its parser continues to populate a
zero AST offset; adding SPARQL paging requires its own frontend contract rather than inheriting GQL
syntax through the shared AST.

### GQL WHERE expression

GQL `WHERE` clauses support `AND` and `OR` expression nodes plus `=`, `!=`, `>`, `<`,
`>=`, `<=`, `LIKE`, `CONTAINS`, `STARTS WITH`, `IN` list literals, and `IS NOT NULL`
predicates. `CONTAINS` and `STARTS WITH` compile to escaped, parameterized `LIKE`
predicates. `IN` list items may be string, integer, finite float, or boolean scalars, and
lists may mix those types. Values are individually bound, and a list containing any string
uses case-insensitive string collation. An empty list compiles to match nothing without
binding value parameters. `NULL` is not a valid list item and is rejected during parsing.
Without `OR`/`IN`, multi-value filters require N separate queries or caller-side UNION.

WHERE references distinguish dedicated substrate fields from JSON properties:

```text
field reference         = variable "." field
JSON property reference = variable ".properties." identifier ("." identifier)*
```

Dedicated fields are checked against the bound substrate's allowlist. Unknown direct fields,
including compiler result aliases such as `_depth`, are errors rather than implicit JSON
lookups. The bare `properties` container is reserved and cannot be compared directly. JSON
access must use the explicit `properties.<path>` form; every path segment is an identifier and
the compiler lowers the complete path through `json_extract`. This makes typos fail closed
while retaining arbitrary and nested JSON-property filters (issues #521 and #1089).

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

Both levels are covered by regression tests in `crates/khive-query/src/` (see `#16`).

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

### Why bounded `SKIP` instead of an opaque cursor or aggregates?

`SKIP` extends the existing GQL window syntax and composes directly with the compiler's
sentinel-based page bound. It works for every currently supported projection without server-side
cursor state or a public cursor encoding tied to multi-substrate identities. Server-side
aggregates answer count questions but cannot retrieve the omitted rows, while `COUNT`/`GROUP BY`
would require a separate expression and grouping model. The compiler-owned identity order is
deliberately not caller-configurable: it supplies the stable continuation contract without adding
the much larger `ORDER BY` surface.

See [Amendment 1][A1], which supersedes this ordering deferral.

## Alternatives Considered

| Alternative                          | Why rejected                                                                                                          |
| ------------------------------------ | --------------------------------------------------------------------------------------------------------------------- |
| Cypher frontend                      | No implementation exists. No concrete use case. GQL covers graph patterns.                                            |
| Cypher output dialect                | No Neo4j backend exists. SQL is the only compilation target.                                                          |
| ATTACH-aware compiler                | Topology is a coordinator concern. Query compiler stays backend-blind.                                                |
| entity_type via JSON extraction      | Bypasses the dedicated indexed column (ADR-001). Wrong query plan.                                                    |
| Endpoint validation in query layer   | Semantic validation belongs in runtime (ADR-003). Query compiles patterns; runtime validates semantics.               |
| Merge khive-query into khive-request | Different grammars, ASTs, and concerns. Coupling creates unnecessary churn.                                           |
| Opaque result cursor                 | Requires a versioned encoding across fixed and recursive result identities; defer until deep-offset cost is measured. |
| Aggregation-only completion          | `COUNT` can audit cardinality but cannot retrieve the missing rows and requires a new expression/grouping model.      |
| Caller-defined `ORDER BY`            | Expands validation and projection semantics; compiler-owned identity order is sufficient for continuation. [A1]       |

[A1]: #amendment-1-edge-metadata-and-bounded-gql-ordering-2026-11-14

## Consequences

### Positive

- Query crate compiles against `khive-types` only — no runtime, storage, or driver dependency.
- `entity_type` queries use the dedicated index.
- Relation validation is automatic via `EdgeRelation::from_str`.
- Adding a new relation to `khive-types` requires zero query-crate changes.
- Read-only constraint prevents query-path bypass of runtime validation.
- Broad GQL results can be enumerated through the query surface without falling back to `list`.

### Negative

- Two parser crates (`khive-request`, `khive-query`) is more surface than one.
  Mitigated: they solve different problems with different grammars.
- Single-database compilation means cross-backend queries require coordinator fan-out
  above the query layer.
  Mitigated: this is the correct architectural boundary per ADR-003.
- Deterministic ordering can require a SQLite sort, and deep `SKIP` pages retain offset-scan cost.
  Mitigated: page payloads remain hard-capped, and a cursor design is deferred until measurements
  show offset cost warrants its additional public contract.
- Offset pages are repeatable only while matching data is unchanged; they are not a cross-call
  database snapshot. Callers requiring a point-in-time audit must prevent concurrent mutation.

### Neutral

- `QueryLanguage` enum retains `Gql` and `Sparql` variants. No enum migration needed.
- Max traversal depth of 10 is a safety bound, not a feature constraint.

## Implementation

- `crates/khive-query/src/parsers/gql.rs`: GQL parser.
- `crates/khive-query/src/parsers/sparql.rs`: SPARQL parser.
- `crates/khive-query/src/ast.rs`: shared AST with `NodePattern.entity_type`,
  `GqlQuery.offset`, and `GqlQuery.order_by` fields.
- `crates/khive-query/src/validate.rs`: AST validation (relation via `EdgeRelation::from_str`,
  depth limits).
- `crates/khive-query/src/compilers/sql.rs`: AST → SQL compilation. `entity_type` maps to
  a column predicate; deterministic identity ordering precedes bound `LIMIT`/`OFFSET`.
- `crates/khive-runtime/src/operations.rs`: sentinel removal and structured continuation
  metadata.
- `crates/khive-pack-kg/src/handlers/graph.rs`: public page-size validation and hard-cap
  enforcement.

## Amendment 1: Edge metadata and bounded GQL ordering (2026-11-14)

**Amendment status**: Accepted 2026-11-14 (#6634)

Issue #6594 requires qualified relations to be read in their stored order without fetching
each edge separately. This amendment supersedes the caller-ordering deferral in Bounded GQL
result pages, its rationale, and Alternatives Considered. The existing read-only,
single-database, visibility, depth and page-size boundaries remain in force.

### Metadata references

Canonical edge bindings support `RETURN e.metadata`, projecting the stored JSON text or SQL
null under `e_metadata`. There is no JSON reserialization or substitution of node properties.
The existing whole-edge `RETURN e` expansion remains unchanged: fixed-length matches
return `id`, `source_id`, `target_id`, `relation`, and `weight`; recursive matches return
the final edge's `id`, `relation`, and `weight`. Metadata requires its explicit projection.
These are the stored field names; whole fixed-edge output keys use `e_source` and
`e_target` for the endpoint fields, preserving the existing wire shape.

`WHERE e.metadata.<identifier>[.<identifier>...]` extracts a JSON value from the canonical
edge's metadata. Every path segment is validated, and the JSON path and comparison values
are parameters. Existing scalar comparison rules apply: exact integers, finite floats,
booleans as integers, case-insensitive text equality and LIKE-family predicates, and literal wildcard escaping for
`CONTAINS` and `STARTS WITH`. Missing and JSON-null leaves yield SQL null. A metadata key
named `namespace` is ordinary stored data, never a visibility override.

Node metadata, edge properties, bare metadata containers in predicates, unknown fields or
bindings, and invalid paths fail before SQL execution. Synthetic `observed_as_*` edges do
not have canonical metadata and reject metadata projection, filtering and ordering. These
rules do not change their existing observation projections or predicates.

### Ordering grammar and value semantics

GQL gains one optional clause between `RETURN` and `SKIP`/`LIMIT`:

```text
ORDER BY reference [ASC | DESC] ("," reference [ASC | DESC])*
reference = variable "." scalar_field
          | variable ".properties." identifier ("." identifier)*
          | variable ".metadata." identifier ("." identifier)*
```

Omitted direction means `ASC`. References must be supported by their bound substrate;
namespace fields and bare `properties`/`metadata` containers are not order terms. This
grammar does not admit functions, expressions, aliases, aggregates, SQL fragments, or a
`NULLS` clause. The new keywords are parsed only in the GQL tail; SPARQL syntax and
identifiers named `order`, `by`, `asc`, and `desc` retain their existing behavior.

Sort expressions retain SQLite's value types without text casts. Ascending order places
null before numeric values, then text under BINARY collation, then blobs; descending order
reverses those groups. JSON booleans become integers, and array/object leaves become JSON
text through `json_extract`. BINARY text sorting is intentionally distinct from the
established case-insensitive text predicate semantics.

Requested terms precede the compiler's existing deterministic tie order. Fixed matches
retain the complete node and edge identity suffix, including substrate discriminators and
synthetic observation primary keys. Each requested direction affects its term only; it
does not reverse the identity suffix. Visibility and deletion filters precede sorting,
sentinel detection, `LIMIT`, and `OFFSET`. Ordering adds no result columns. Continuation
uses the same query, ordering and page size with the returned `next_offset`; the explicit
query `LIMIT` remains the total bound across pages. Offset paging does not provide a
cross-call snapshot when matching data changes.

### Execution cost and cancellation

Caller-selected JSON metadata sort paths have no dedicated sort index. Producing a page
can require evaluating and ordering the whole visible match set before `LIMIT` and
`OFFSET`; deep ordered reads can repeat a full sort per page rather than resume an
indexed cursor. `CompileOptions.max_limit` defaults to 500 and caps the returned page,
with at most one extra internal continuation row; it does not cap the matched set or the
sort's work. The public `query` handler accepts a requested page size up to 10,000. A
query-text `LIMIT` bounds returned results across pages, not the number of matching rows
SQLite must examine to order them.

Built-in MCP hosts scope reads to the request deadline and cancellation signal. For a
query-only request, `KHIVE_REQUEST_READ_TIMEOUT_SECS` defaults to 30 seconds and accepts
1 through 86,400 seconds; invalid values use the default and larger values are clamped.
The request policy can extend a batch's allowance for `comm.inbox` waits, and nested
deadlines retain the earlier bound. Daemon dispatch also installs its configured read
deadline and cancels reads on peer disconnect or shutdown. These contexts reach
`query_with_metadata` and `SqlReader::query_all`: both file-backed and in-memory built-in
SQLite readers register the running read statement's exact connection for interruption,
with a connection-local progress callback checking cancellation and deadline expiry.

This is cooperative cancellation, not an unconditional execution-time or memory bound.
Interrupt settlement may extend beyond the request deadline; uncooperative blocking
work can outlive the settlement bounds and retain its connection after detachment.
Direct Rust callers of `query_with_metadata` inherit an existing request context but do
not acquire a deadline from that method. Custom `SqlAccess` implementations must provide
their own cancellation behavior. The query compiler itself imposes no matched-row,
sort-work or elapsed-time cap.

### Recursive final-edge semantics

For a variable-length path, edge metadata describes the final traversed edge. Both CTE
arms carry `via_metadata` only when projection, filtering or ordering needs it. A final-edge
metadata predicate runs after traversal, so metadata on an earlier hop cannot prune a
path that has a matching final edge. Final-edge and end-node predicates share that phase
and preserve their boolean grouping. An `OR` crossing start-node and final-row phases
remains explicitly unsupported; an `AND` can be routed to both phases.

Recursive results retain `DISTINCT`. Each requested sort value must be derivable from the
actual returned projections, so collapsing identical projected rows cannot leave an
ambiguous hidden sort value. For example, `RETURN e.metadata ORDER BY e.metadata.order`
is valid. `RETURN e ORDER BY e.metadata.order` is not: the whole-edge expansion does not
include metadata. Likewise, a whole-node projection does not implicitly include fields
absent from its documented expansion. Non-derivable terms return `Unsupported` with
`variable-length ORDER BY must be derived from RETURN projections`. A returned JSON
container permits ordering by its named paths. A returned start-node sort value uses the
start-row join. The requested prefix precedes the existing depth, descending accumulated
weight, and projected-value tie order, without adding hidden columns to `DISTINCT`.

### Compatibility and acceptance

The shared AST adds `GqlQuery.order_by: Vec<OrderItem>`, typed `OrderDirection::{Asc, Desc}`,
and `PropertyRef::MetadataPath(Vec<String>)`. This breaks direct Rust `GqlQuery` constructors:
they must initialize the new field, and exhaustive property-reference matches must
accommodate the new variant. The only known in-tree consumers of `khive-query` are
`khive-runtime` and `khive-pack-kg`; this does not establish compatibility for external
Rust consumers. SPARQL constructs an empty order list.
Compilation validates hand-built AST paths and fields as well as parsed input. No storage
migration, runtime production change, or new dependency is required.

When neither metadata nor ordering is requested, compiled SQL, parameter values and
indices, and outward result shapes remain unchanged. Conditional CTE metadata carriage
must preserve both old CTE arms exactly. Acceptance requires actual stored-value and
canonical query-dispatch tests for raw JSON, mixed scalar types, final-edge predicates,
ascending/descending order, identity ties, multi-page reconstruction, total limits,
visibility, unchanged domain state, recursive `DISTINCT`, and typed refusals. Compiler
tests also cover direct-AST injection attempts, old SQL/parameter compatibility and
unchanged SPARQL identifiers. Mutation controls must fail observable behavior assertions,
not merely fail to compile or time out.

Comma-separated `MATCH` patterns remain unsupported under this amendment. Their separate
design and implementation follow the primary metadata/ordering change; #6594 remains open
until both parts are delivered.

## Amendment 2: Connected comma-separated MATCH patterns (2026-11-14)

**Amendment status**: Accepted 2026-11-15 (#6654)

This amendment completes the comma-separated `MATCH` portion of #6594 with connected,
fixed-length canonical graph patterns. It replaces Amendment 1's comma-pattern deferral
only for the form defined here. Single-pattern recursive and synthetic traversal retains
its existing contract; this amendment does not add general Cypher compatibility.

### Grammar and supported shape

```text
MATCH path ("," path)* [WHERE ...] RETURN ... [ORDER BY ...] [SKIP ...] [LIMIT ...]
```

There is one global `WHERE`, `RETURN`, `ORDER BY`, `SKIP`, and `LIMIT` tail. With more than
one path, every edge must be canonical and fixed at one hop (`min_hops == max_hops == 1`).
A path may contain several such edges: the restriction is on each edge's hop range, not
on the number of edges in a path. Empty or trailing comma components are parse errors.
Directions and canonical relation alternatives retain their existing meanings.

All paths together must form one connected set through shared named node variables.
Connectivity is checked over the complete set, so a later path may bridge two earlier
paths. Anonymous node occurrences are fresh and cannot connect paths. A singleton node
path is permitted when its name connects it to the other paths and adds constraints;
a disconnected singleton is refused.

Disconnected patterns, recursive multi-pattern matches, and multi-pattern matches using
synthetic `observed_as_*` relations return an explicit `QueryError::Unsupported`. A second
`MATCH` keyword, `OPTIONAL MATCH`, `UNION`, projection aliases, aggregates, and new `DISTINCT`
syntax remain unsupported. Existing single-path recursive, synthetic, and SPARQL queries
retain their current behavior; SPARQL branching remains unsupported.

For example, this star has one shared center and independently bound edges and endpoints:

```gql
MATCH (a:concept)-[e:extends]->(b:concept),
      (a)-[f:variant_of]->(c:concept)
WHERE e.metadata.order >= 0 AND f.metadata.enabled = true
RETURN a.id, b.id, c.id, e.id, f.id, e.metadata, f.metadata
ORDER BY e.metadata.order ASC, f.metadata.order DESC
```

### Shared identities, constraints, and multiplicity

A named node repeated across paths denotes the same typed identity: both its substrate
kind and UUID must agree. UUID equality alone does not identify an entity with a note.
Variable names remain case-sensitive. Every occurrence's kind, subtype, and inline-property
constraints is conjunctive. A later occurrence cannot replace an earlier constraint;
contradictory constraints produce an empty match set. Ordinary field and property
comparison and missing-value semantics are unchanged.

The existing refusal of a repeated node variable within one path remains. Repeating an
edge variable anywhere in the query is refused, as is using the same name for a node and
an edge. Different variable names do not require different records. In particular, two
edge variables may select the same edge; no implicit edge inequality is introduced.
Cross-path cycles through shared named nodes are permitted without adding within-path
cycle syntax.

Each path contributes a conjunctive match, and results retain bag multiplicity. A shared
center with two matches on one branch and three on another yields six binding tuples.
Repeated path shapes remain independent matches; neither paths, edge identities, nor
equal projected rows are deduplicated.

A legal multiplicity witness uses the same two concept endpoints joined by two distinct
natural keys: edge `E` with relation `extends` and edge `V` with relation `variant_of`.

```gql
MATCH (a)-[e1:extends|variant_of]->(b),
      (a)-[e2:extends|variant_of]->(b)
RETURN e1.id, e2.id
```

The result contains exactly `(E,E)`, `(E,V)`, `(V,E)`, and `(V,V)`. Returning only the shared
node identity instead still returns four equal projected rows. This does not require
duplicate stored edge natural keys. Reusing `e1` as the second edge variable is refused.

### References, filters, and visibility

The global `WHERE`, `RETURN`, and `ORDER BY` clauses can reference bindings from any path.
A shared node reference resolves through its first occurrence, while constraints from
every occurrence remain active. The original global `AND`/`OR` expression tree is retained;
the compiler must not prune or distribute it into separate per-path expressions. This
amendment adds no parenthesized predicate grammar.

Amendment 1's metadata and ordering rules apply to every canonical edge binding, including
later paths: raw whole-metadata JSON text, bound metadata predicate paths, typed ordering,
and binary string comparison remain unchanged. `RETURN e` still does not implicitly add
metadata. Properties and metadata namespace values remain data, not authorization inputs.

Every node and edge occurrence uses the existing filtered node and edge sources. Caller
namespace visibility, live-row filtering, and mailbox visibility apply before matching,
ordering, and page selection, including on later branches. The caller scope is bound once
and reused by all paths. Hidden endpoints or edges cannot consume a visible result's page
slot or sentinel. The query remains a read within one selected backend.

### Ordering, continuation, and cost

Caller ordering terms precede a deterministic identity suffix covering every occurrence
in textual path and element order. A node contributes its substrate kind before its UUID;
an edge contributes its UUID. Shared-node occurrences may contribute redundant identities.
Requested `DESC` directions do not reverse this identity suffix.

The existing `SKIP`, sentinel, total query `LIMIT`, and `next_offset` contract is unchanged.
The total query limit spans pages rather than restarting at each offset. Zero limits and
offsets beyond the result return no rows. Paging remains deterministic only while the
matching data is unchanged; it is not a snapshot shared across requests.

Connectedness does not bound intermediate fanout. Match combinations and sorting work can
greatly exceed the returned page size. Amendment 1's cost and cancellation qualifications
continue to apply: page bounds do not bound the matched set, and deep ordered pages can
repeat evaluation and sorting of the full visible matching set. This amendment introduces
no additional hard bound on matched rows, elapsed time, CPU, or memory, and no universal
interrupt guarantee for custom backends.

### AST and compatibility

`GqlQuery.pattern` remains the required first `MatchPattern`. The AST adds
`additional_patterns: Vec<MatchPattern>` plus ordered `patterns()` and `patterns_mut()`
iterators over the first and additional paths. An empty additional vector represents the
existing single-path query. Empty additional paths are refused, and the existing refusal
of an empty primary pattern remains.

Adding this public field intentionally breaks direct Rust `GqlQuery` struct literals:
callers must initialize `additional_patterns`. The GQL parser, SPARQL parser, and malformed
AST tests must be adapted; SPARQL constructs an empty additional vector. Parsed single-path
queries require no syntax change. Directly constructed ASTs receive the same connectivity,
variable-kind, metadata-path, namespace, and supported-shape checks as parsed queries.

The fixed-pattern compiler uses globally unique SQL aliases and resets only the predecessor
at each path boundary. Later path roots use the existing filtered node source, with typed
shared-identity predicates joining the component matches. References retain the first
alias; all occurrence constraints survive. Existing parameterization rules remain in force.

With no additional paths, generated SQL and parameters remain byte-identical for existing
fixed, recursive, synthetic, and SPARQL queries, including Amendment 1's metadata and order
behavior. Runtime execution remains one compiled SQL read with the existing sentinel
handling. Static endpoint warnings inspect every path using the existing warning rules;
they add no new satisfiability inference. No schema, storage, gate, dependency, or taxonomy
change is required.

### Validation requirements

1. Execute the star query through the runtime and the actual KG registry. Require exactly
   six tuples, exclude wrong-center decoys, and prove a nonempty narrowed `WHERE` result,
   raw metadata projection, and unchanged populated domain rows.
2. Prove typed shared identity with a trusted-store fixture containing an entity and a
   note with the same UUID. This fixture does not claim public create permits the collision.
   Cover equal, complementary, and contradictory occurrence constraints; shared starts and
   ends; case-sensitive and anonymous names; reversed component order; and a later bridge.
3. Execute the two-relation multiplicity witness above, asserting all four exact edge-ID
   pairs and all four equal node projections. Cover directions, relation alternatives,
   anonymous endpoints, later inline filters, global `AND`/`OR`, and later-path scalar and
   metadata references.
4. Place requested metadata order opposite UUID order, include tied groups, and verify
   both directions and the fixed identity suffix. Reconstruct all tuples with page size
   two, enforce total query limit five, and check zero limits, exhausted offsets, sentinel
   removal, and `next_offset`.
5. Through actual KG and Comm packs, place hidden mailbox rows on a later branch and keep
   a visible positive. Reverse branch order and independently cover deleted or hidden
   edges and endpoints ahead of visible matches. Retain the existing mailbox regressions.
6. Test parser and direct-AST refusals for disconnected, recursive, synthetic, empty,
   repeated-edge, node/edge-collision, forbidden-namespace, unknown-variable, and malformed
   metadata-path cases. Keep real single-path recursive and synthetic positive controls.
7. Preserve exact single-path SQL and parameters, recursive `DISTINCT` constraints, SPARQL
   behavior, and later-path static endpoint warnings. Compare domain snapshots separately
   from any permitted read-audit events.

Behavioral negative controls must independently omit shared identity, omit its substrate
component, drop a later occurrence's inline predicate, deduplicate tuples, ignore requested
ordering, reverse the identity suffix, remove later-path mailbox filtering from both node
and edge sources, and bypass direct-AST connectivity validation. Each control must reach
its intended result or refusal assertion; compilation failures, empty test selections,
fixture failures, and timeouts do not establish the behavior.

## Amendment 3: GQL pages, projections, aggregation and bounded path composition (2026-11-15)

**Amendment status**: Proposed

GQL must support complete paged answers, scalar property projections, counts, distinct
results, absence queries, and paths that combine bounded traversal with fixed steps. This
amendment defines those forms for issues #6691, #6690, #6689, #6687, #6688 and #6686.

It supersedes the total-query `LIMIT` statements in Amendments 1 and 2, and their deferrals
of the specific projection, aggregation and path forms defined below. Their remaining
contracts and recorded statuses are unchanged. Read-only execution, one selected backend,
caller visibility, live-row filtering, mailbox restrictions and bounded response pages
continue to apply.

### Page size after SKIP

`SKIP n LIMIT m` skips the first `n` ordered result rows and then returns at most `m` rows.
The offset is not subtracted from `LIMIT`. `LIMIT 0` and offsets beyond the result return
no rows. The server's `page_size` still caps the payload, so the emitted bound is the
smaller of the explicit limit and the server page size.

When the server page size is the binding bound, one internal continuation row determines
`has_more`; that row is not returned. `next_offset` advances by the number of emitted rows.
An explicit limit at or below `page_size` remains a terminal caller-selected page: no
continuation is advertised beyond that limit. `has_more: false` in this case does not claim
that the underlying match set is exhausted. `truncated` retains its compatibility meaning.
Following `next_offset` applies the same `LIMIT` after the new offset; there is no separate
cumulative query limit across requests.

For twenty ordered rows numbered 1 through 20, `SKIP 10 LIMIT 3` returns 11, 12 and 13.
With `LIMIT 5` and `page_size: 2`, offsets 0, 2 and 4 each return two rows and advance to
2, 4 and 6 while another row exists. Offset pages remain repeatable only while the matching
data is unchanged; they do not share a snapshot across requests.

### Zero-hop traversal

A supported variable-length segment accepts a minimum of zero, including `*0..0` and
`*0..1`. At depth zero its start and end denote the same typed node identity, its accumulated
weight is zero, and its final-edge binding is null. Every final-edge projection, including
ID and metadata paths, is therefore null. The row does not borrow an incident edge.
Start and end constraints both apply, together with the ordinary visibility and deletion
checks. An isolated node can match a zero-hop segment; a minimum of one excludes that row.

The existing maximum depth, inverted-range refusals and cycle restrictions remain. Zero-hop
semantics also apply to equivalent explicit ranges already expressible through the shared
SPARQL path representation; this adds no SPARQL paging or new SPARQL syntax.

### JSON path projections and result column names

`RETURN` accepts the same typed paths as scalar predicates:

```text
variable.properties.identifier[.identifier ...]
edge.metadata.identifier[.identifier ...]
```

The first form requires a node binding; metadata requires a canonical edge binding. Each
path segment is validated, and the complete JSON path is a bound value. Unknown bindings,
invalid path segments, node metadata and synthetic-edge metadata are refused before
execution. Whole-object projections retain their existing result shape and stored JSON text.

Path results retain the existing JSON extraction types: integers and finite numbers remain
numeric, strings remain text, booleans are 0 or 1, and JSON null or a missing leaf becomes
null. An array or object at the selected leaf is JSON text. A variable segment's edge path
reads its final edge, including the null final edge at zero hops.

Column names are deterministic: `f.properties.status` becomes `f_properties_status`, and
`e.metadata.rank.order` becomes `e_metadata_rank_order`. Segments are joined with underscores
without case folding. Different expressions that produce the same column name are refused;
for example, `n.properties.a_b` and `n.properties.a.b` cannot both be returned under
`n_properties_a_b`. Existing direct-field and whole-binding column names remain unchanged.
These generated names are output labels; this amendment does not introduce `AS` or
`ORDER BY` references to output aliases.

### DISTINCT, count and grouping

The supported additions are `RETURN DISTINCT ...` and `count(variable)`. The count operand
must be a bound node or edge, not a property expression. Other returned expressions form
the implicit grouping keys; their returned values, rather than unreturned record identities,
define the groups. With only counts, an empty match set returns one row containing zero
for each count. With grouping keys, an empty match set returns no rows. `count(x)` is named
`count_x`, subject to the same output-column collision check.

Counts operate on matched binding rows before the ordinary recursive projection's existing
deduplication. Repeated occurrences of one node or edge in different matching paths count
separately. A null edge binding at zero hops contributes zero to `count(edge)`; the bound
node contributes one to `count(node)`. Synthetic edge bindings use their existing composite
identity and do not require a canonical edge ID. This adds neither `count(*)`,
`count(DISTINCT ...)` nor other aggregate functions.

Filtering and complete path matching precede projection and grouping. Explicit `DISTINCT`
applies to the requested result values after any grouping, before ordering, `SKIP`, payload
limits and continuation detection. Grouped and explicit-distinct results omit the implicit
`_depth` and `_total_weight` columns. Those metrics must not secretly split a group or defeat
deduplication. Ordinary recursive results retain their existing implicit metrics and
projection-deduplication behavior.

Existing typed extraction and ordering comparisons apply. Missing and JSON-null values form
one null group; text uses binary comparison, while numeric values retain numeric comparison.
Requested ordering on distinct or grouped output must be derivable from its actual returned
values. Hidden sort expressions remain refused. Deterministic ties use the returned result
values, without adding unreturned binding identities to grouping or deduplication.

For two distinct matching paths to one endpoint, `RETURN DISTINCT b.id` returns that endpoint
once; `RETURN count(b)` returns 2. At an isolated root, a zero-hop query returning
`count(a), count(e)` returns 1 and 0.

### Correlated absence predicates

GQL supports one absence form:

```gql
MATCH (r:concept)
WHERE r.name STARTS WITH "requirement:"
  AND NOT EXISTS { (f:concept)-[:supports]->(r) }
RETURN r.name
```

A `NOT EXISTS` block contains one canonical fixed-length path and references at least one
outer node binding. It may contain multiple fixed steps and ordinary inline constraints.
A reused outer node name correlates both substrate and UUID; it does not introduce a new
node or compare UUID alone. Additional inner names are local to the block and cannot be
returned or referenced outside it. Node/edge name conflicts and attempts to correlate an
outer edge as a node are refused. Ordinary fixed-pattern variable rules still apply.
An inner edge variable that is already bound as an outer edge is refused at compile time,
with an error naming the variable and stating that edge variables are not correlated into
`NOT EXISTS`; the name cannot shadow the outer binding.

The predicate is true exactly when no caller-visible inner binding satisfies the path.
Inner nodes and edges use the same namespace, live-row and mailbox restrictions as the
outer query. Hidden, deleted, out-of-scope or malformed-mailbox support cannot suppress a
visible outer result. The block is one predicate atom under the existing `AND`/`OR` rules;
it can correlate to the endpoint of an outer variable-length path. Absence filtering occurs
before grouping, ordering and pagination.

This form does not add `OPTIONAL MATCH`, null-extended outer rows, inner query clauses,
nested absence blocks, recursive inner paths or synthetic inner edges. Unsupported forms
receive explicit refusals rather than a partial interpretation.

### Mixed fixed and variable-length paths

One canonical path may alternate fixed steps and bounded variable-length segments:

```gql
MATCH (a:document)-[:depends_on*0..4]->(b:document)-[:derived_from]->(s:document)
WHERE a.name = "x"
RETURN s.name
```

All intermediate nodes, directions, relation alternatives and constraints participate in
the match. A zero-hop segment joins its start node directly to the following step, so the
example includes documents attached to the root itself. Each variable segment's edge
binding identifies that segment's final edge; a later fixed edge does not replace it.

Mixed paths evaluate the full `WHERE` Boolean expression on the complete joined row before grouping and paging, including `OR` across fixed-step and variable-segment bindings; the existing single-segment cross-phase `OR` refusal is unchanged.

For ordinary recursive output, `_depth` sums the actual hops of the variable-length
segments and `_total_weight` sums the weights traversed within those segments. Fixed steps
contribute to neither metric. Zero-hop segments contribute zero to both. Cycle avoidance
continues within each variable segment; it does not introduce a new global path-uniqueness
rule across segments.

The sum of the maximum hop bounds of all variable-length segments in one path must not
exceed the existing maximum traversal depth of 10. Fixed steps remain finite joins. The
bound is validated before execution, including directly constructed queries. Connected
comma-separated matches retain Amendment 2's fixed-length canonical restriction; this
amendment does not add recursive or synthetic comma-separated matches.

### Compatibility and acceptance

The new `LIMIT` meaning intentionally changes callers that treated it as a cumulative
budget. Zero-hop ranges and the specified syntax become accepted; unrelated unsupported
forms remain refused. Ordinary recursive output retains its legacy shape, while callers
requesting distinct or aggregate output receive only their requested columns. Public query
representations must validate the same rules whether parsed or constructed directly.
There is no storage migration or additional query frontend.

Acceptance requires actual stored-result and public query-dispatch witnesses:

1. Prove post-offset limits, zero and exhausted pages, sentinel removal and continuation
   with hidden rows ahead of visible rows, including connected comma matches.
2. Prove isolated and connected zero-hop roots, both endpoint constraints, null final-edge
   values, directions, cycles and equivalent existing SPARQL ranges.
3. Return nested numeric, text, boolean, missing and null JSON leaves; retain whole-object
   projections; refuse unsafe paths and ambiguous output labels.
4. Prove grouped counts, count-only and grouped empty inputs, repeated path bindings,
   zero-hop edge exclusion, and distinct/grouped pagination independent of hidden metrics.
5. Prove visible support suppresses an absence result while hidden, deleted and inaccessible
   support does not; cover typed correlation, local-variable refusal and boolean composition.
6. Prove fixed-prefix, fixed-suffix and multiple-variable chains, root-following fixed steps,
   each segment's final-edge metadata, intermediate visibility and cumulative-depth refusal.

Result bounds do not bound intermediate fanout, sorting work, CPU or memory. Existing
cancellation qualifications and offset-paging costs remain. Acceptance also preserves the
read-only boundary and existing successful fixed, recursive, synthetic and SPARQL cases;
behavioral controls must fail their intended result or refusal assertions.
