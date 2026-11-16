# SQL Compilation

The SQL compiler lowers a validated `GqlQuery` into a read-only, parameterized `CompiledQuery`. Fixed-length patterns use JOIN chains; a single variable-length edge uses a recursive CTE with cycle prevention and deterministic result ordering.

## `compile`

`compile(query, options)` validates the AST, selects the fixed- or variable-length lowering path, and returns SQL, ordered parameters, return projections, warnings, and optional truncation metadata. It can return any `QueryError` raised by validation or lowering; it never executes the SQL.

As defense in depth, the final statement must begin with `SELECT` or `WITH`. Parser guards already reject writes, and the database reader is independently read-only; this compiler check prevents a future lowering path from emitting a mutation by mistake.

## `compile_with_mailbox`

`compile_with_mailbox(query, options, mailbox)` additionally applies a
`QueryMailboxScope { actor_id, legacy_local }` supplied by the runtime. This is a
pure compilation API: it does not authenticate callers or authorize delegation.
The runtime derives the partition from its caller identity; query text cannot
select another mailbox. The original `compile` entry remains available for
unscoped SQL construction.

The partition is applied to note rows in both the primary-node union and typed
observation targets. Ordinary note kinds remain visible. Message rows require a
text `to_actor` matching the caller for `direction = "inbound"`, or a text
`from_actor` matching the caller for `direction = "outbound"`. JSON arrays,
objects, numbers and booleans cannot impersonate text actor labels. Only the
anonymous, non-delegated local caller receives the existing legacy partition:
missing/null routing for the relevant direction, or missing/null direction with
neither actor field holding text. A named actor whose label is `local` does not
receive that legacy access.

Canonical edges are filtered at every source: edge rows projected as nodes,
fixed joins, and both the seed and step of recursive traversal. For each endpoint,
an entity with the same UUID takes precedence, including a deleted entity and
regardless of its namespace. If no such entity exists, a message note with that
UUID must pass the mailbox partition, including when the note is deleted. Other
endpoints pass this mailbox check. Missing routing is a refusal rather than SQL
`NULL` escaping the edge anti-join. This identity check does not make deleted or
out-of-scope node rows queryable; their existing row filters still apply.

Actual note rows are independently filtered even when an entity shares their
UUID. Synthetic observation targets keep their explicit `referent_kind`, so an
entity referent cannot expose the equal-ID hidden note. Event payloads are not
recursively inspected or redacted by this partition.

The actor is bound once as a text parameter and reused across source predicates.
Visibility filtering precedes matching, ordering, `DISTINCT`, `LIMIT` and `OFFSET`,
including intermediate recursive nodes. Paging and sentinel counts therefore
range over the filtered result. Namespace options and query limits are unchanged.

This compiler operates on one database. Generic graph endpoint resolution can
search all configured backends. If an entity exists only in another backend,
the single-database query can withhold an edge that a generic graph read permits.
This API does not add cross-backend query federation.

## `CompileOptions`

`scopes` supplies namespace filters. An empty vector means cross-namespace; otherwise every applicable table is filtered with bound parameters. Query text cannot supply this value.

`max_limit` is the effective per-call page size supplied by the runtime after applying its hard cap. An explicit query `LIMIT` is a total bound across pages: the remaining allowance is `max(LIMIT - SKIP, 0)`, and the payload bound is the lesser of that allowance and `max_limit`, subject to checked `usize`-to-`i64` conversion. Without an explicit limit, the payload bound is `max_limit`. Query-verb callers normally set this through `page_size`; its public range is 1 through 10,000 rows.

`GqlQuery.offset` is emitted as a bound SQL `OFFSET`. It must fit SQLite's signed integer range. A negative GQL `SKIP` is rejected by the parser, and a hand-built or platform-sized value above `i64::MAX` is rejected before execution.

## Truncation sentinel

When the remaining explicit allowance `max(LIMIT - SKIP, 0)` is at or below `max_limit`, the caller's limit is terminal and the compiler fetches at most that allowance, without a sentinel. When there is no explicit limit or the remaining allowance exceeds the page size, SQL fetches `max_limit + 1` rows and sets `CompiledQuery.truncation_check`. For example, `SKIP 90 LIMIT 100` with page size 50 fetches at most ten rows and no sentinel.

The execution site removes the sentinel and truncates to `TruncationCheck.max_limit`. `requested_limit` retains the caller's explicit value for diagnostics. Inspecting the extra row avoids both false warnings when a large limit matches few rows and silent truncation when an unbounded query matches more than the page size (issue #777). For GQL, a sentinel produces `has_more: true`, retains `truncated: true` for compatibility, and returns `next_offset = current offset + emitted rows`; terminal pages omit `next_offset`.

## Parameter and property binding

All scope values, relation filters, property values, depths, and limits are bound parameters. Integer values remain `INTEGER`, finite decimals remain `REAL`, booleans use integer `0`/`1`, and text equality uses `COLLATE NOCASE`. Non-finite floats in hand-built ASTs return `InvalidInput`.

Inline property equality continues to map arbitrary keys through `json_extract`. In `WHERE`, dedicated fields map directly to columns. Explicit node `properties.<path>` references extract from properties; canonical edge `metadata.<path>` references extract from metadata. Metadata paths and comparison values are bound parameters; existing node-property predicates retain their validated path literals. Unknown direct fields and bare containers are rejected; the same substrate-aware resolver is used by fixed- and variable-length compilation. Event predicates require a known event column. Existing direct edge predicates accept `relation` and `weight`, and edge JSON predicates require the metadata root, never properties. The `entity_type` field always uses its dedicated column. Literal wildcard characters are escaped before compiler-supplied `LIKE` wildcards are added.

`RETURN e.metadata` reads the stored JSON text or SQL null without reserialization. Missing and JSON-null extracted leaves both become SQL null. Integers, floats and booleans retain their SQLite scalar representations; extracting metadata does not coerce every value to text. Metadata access on nodes or synthetic observation edges is rejected before SQL generation.

Node kind labels `entity`, `note`, `event`, and `edge` select a substrate in the primary-node union; granular values such as `concept`, `task`, or comm's pack-registered `message` filter the stored `kind`. No stored row is expected to have the literal granular kind `entity` (issue #849).

Granular kind filtering is intentionally data-driven. The compiler has no `VerbRegistry`
dependency and does not keep a second allowlist of pack-owned kinds: registration validates the
write, while GQL binds the stored kind as a SQL parameter. `MATCH (m:message)` and
`MATCH (m:note) WHERE m.kind = "message"` therefore select the same caller-visible message
notes. GQL's field is `kind`; `note_kind` is terminology from the verb parameter surface and is
not a query field. Runtime-supplied namespace scopes still apply to every union member.

## Fixed-length JOIN compilation

Canonical edges bind through `graph_edges`, while endpoint nodes bind through a union of entities, notes, events, and graph edges. This substrate-agnostic source is necessary because relations such as `annotates` can target several substrates and epistemic relations can connect notes (issue #467).

The compiler preserves edge direction, filters soft-deleted rows, applies namespace scope to every bound substrate, preserves `AND`/`OR` grouping, and resolves WHERE predicates and RETURN projections against the bound variable's substrate. Fixed-length results append every bound node and edge identity after any requested ordering terms, before applying `LIMIT`/`OFFSET`; substrate discriminators precede UUIDs so equal IDs in different union members remain distinct, and synthetic observations use their `(event_id, role, position)` primary key.

### Connected comma-separated paths

The fixed compiler walks all paths in textual order with globally unique node and edge aliases. Each later root adds a `CROSS JOIN` of the same filtered node source, followed by its ordinary edge/node joins. Shared node variables add both substrate and UUID equality predicates; disconnected components are refused before lowering. References retain the first alias, and every occurrence keeps its own filters. Namespace, deletion and mailbox predicates apply to every occurrence before matching and paging, with the caller actor parameter bound once and reused.

The result is the conjunction of path matches with bag multiplicity. Two matches on one branch and three on another yield six tuples. Distinct edge variables may select the same edge; neither paths nor equal projected rows are deduplicated. There is one global WHERE tree, projection, order and paging tail. The identity suffix covers every occurrence in textual path/element order, with substrate before node UUID and edge UUID at each edge occurrence. Requested DESC terms do not reverse that suffix.

An empty additional-pattern vector preserves existing fixed, recursive, synthetic and SPARQL SQL and parameter bytes. Multi-pattern queries execute as one compiled read; no Rust-side joining or per-path paging is introduced. Connectedness does not bound intermediate fanout or sorting work. Page bounds remain result bounds, and cancellation and unchanged-data continuation qualifications from [ADR-008 Amendment 1](../../../../docs/adr/ADR-008-query-layer-separation.md#amendment-1-edge-metadata-and-bounded-gql-ordering-2026-11-14) continue to apply.

## Caller ordering and continuation

`ORDER BY` resolves validated scalar fields or named node-properties/canonical-edge-metadata paths. It emits no extra result columns. Bare containers and namespace fields are rejected; direction is a typed `Asc` or `Desc`, never caller-supplied SQL. Queries using neither metadata nor ordering retain unchanged SQL and parameters.

Sort values are not cast: ascending SQLite order is null, numeric, BINARY-collated text, then blob; descending reverses this value order. JSON array/object leaves remain JSON text. Predicate equality can still use NOCASE collation, so matching text case-insensitively does not imply case-insensitive sorting. Each requested direction applies only to its term; the existing identity tie suffix retains its direction.

All visibility and deletion predicates apply before ordering and the page sentinel. The runtime's effective limit preserves a query-text `LIMIT` as a total across `SKIP` pages. To continue, retain the query and order terms, replace `SKIP` with `next_offset`, and keep the page size. Repeated reads of unchanged data are stable; concurrent changes can shift an offset page.

## Synthetic observation edges

The four `observed_as_*` relations bind through `event_observations`, not `graph_edges`. Their source is an event; their target is the entity/note referent union, discriminated by `referent_kind` so equal UUID bytes on different substrates cannot collide (issue #468).

Legal role/target pairs are:

| Relation role           | Target substrate |
| ----------------------- | ---------------- |
| `candidate`, `selected` | note             |
| `target`, `signal`      | entity or note   |

Synthetic edges must be outbound and fixed-length. Mixing synthetic and canonical relations in one edge is rejected because the backing tables have no meaningful shared join key. Event nodes expose event columns and reject `entity_type` or arbitrary property maps; observation targets expose the common entity/note projection columns.

## Variable-length recursive CTE

Variable-length compilation currently accepts exactly one `start -[*N..M]-> end` pattern. Mixed fixed/variable chains, trailing elements, and synthetic edges return `Unsupported`.

The recursive seed starts at depth one. The CTE binds maximum and minimum depth as parameters, records visited IDs in a path string to prevent cycles, filters deleted and out-of-scope intermediate nodes, and accumulates edge weight. Because the final projection uses `DISTINCT`, its deterministic tie order is depth and descending total weight, then every projected output alias. That tuple is total over the rows that survive `DISTINCT` and avoids choosing an arbitrary hidden path as the representative for a collapsed row.

Requested order terms precede that tuple only if derivable from the actual returned projections. A returned JSON container permits ordering by a path within it; an unreturned field does not. Whole-edge projections omit metadata, and whole-node projections do not implicitly return description. Non-derivable terms return `Unsupported` containing `variable-length ORDER BY must be derived from RETURN projections`. The compiler does not add hidden fields to the DISTINCT projection to make an otherwise ambiguous order legal.

Canonical edge metadata describes the final traversed edge. The CTE declaration, seed and step carry `via_metadata` only when RETURN, WHERE or ORDER BY needs it; old queries retain their exact CTE SQL and parameters. Final-edge metadata predicates run after traversal, alongside end-node predicates, so an earlier edge's metadata cannot incorrectly prune the path.

The end-node union is always joined because end filters apply even when the end variable is not projected. A start-node join is emitted only when its columns are returned.

`OR` within one phase is preserved, including a final-edge metadata predicate combined with an end-node predicate. An `OR` spanning start-node and final-row phases is rejected because routing its halves to separate CTE phases would silently change it into `AND`; cross-phase `AND` remains supported.

## Projection columns and failure modes

Whole-variable projections expand to the allowed columns for their substrate. Property projections are checked against node, event, observation-target, or edge whitelists; unknown columns return `QueryError::Compile`. Fixed-length `RETURN e` retains `id`, `source_id`, `target_id`, `relation`, and `weight`; recursive `RETURN e` retains the final edge's `id`, `relation`, and `weight`. Metadata requires explicit `RETURN e.metadata`. Edge predicates accept direct `relation`/`weight` or explicit metadata paths. Synthetic observation edges reject metadata projection, predicates and ordering.

Common unsupported cases include repeated node variables within a path, repeated edge bindings, disconnected comma components, recursive or synthetic multi-pattern matches, zero-hop paths, mixed fixed/variable chains, SPARQL `*`, variable-length synthetic edges, and `OR` spanning both variable-length endpoints. Limits or depths that cannot fit the SQL parameter type return `InvalidInput` rather than wrapping.
