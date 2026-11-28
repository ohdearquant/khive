# SQL Compilation

The SQL compiler lowers a validated `GqlQuery` into a read-only, parameterized `CompiledQuery`. Fixed-length patterns use JOIN chains; variable-length segments use recursive CTEs with cycle prevention and deterministic result ordering. Mixed canonical paths compose those segments and fixed joins before filtering and paging.

## `compile`

`compile(query, options)` validates the AST, selects the fixed- or variable-length lowering path, and returns SQL, ordered parameters, return projections, warnings, and optional truncation metadata. It can return any `QueryError` raised by validation or lowering; it never executes the SQL.

As defense in depth, the final statement must begin with `SELECT` or `WITH`. Parser guards already reject writes, and the database reader is independently read-only; this compiler check prevents a future lowering path from emitting a mutation by mistake.

## `compile_with_mailbox`

`compile_with_mailbox(query, options, mailbox)` additionally applies a
`QueryMailboxScope { actor_id, legacy_local, maintainer_actor }` supplied by the runtime. This is a
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

A nonempty `maintainer_actor` changes ownership only for message rows whose
`quarantined` property is JSON `true` or the exact JSON string `"true"`. Those
messages belong to the configured maintainer, independent of their stored sender,
recipient or direction. Every other message keeps the ordinary partition above;
non-message notes are unchanged. Numeric `1`, `1.0`, other strings, arrays, objects,
JSON null and a missing property are ordinary values. `None` and an empty
maintainer label preserve the previous partition exactly. The runtime supplies
this trusted configuration separately from the caller; query text cannot appoint
a maintainer. Visibility does not rewrite stored routing or quarantine values.

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
A nonempty configured maintainer is bound once as a second text parameter; neither
label is interpolated into SQL. The quarantine branch and its ordinary fallback
produce a total boolean even when JSON fields are missing or null.
Visibility filtering precedes matching, ordering, `DISTINCT`, `LIMIT` and `OFFSET`,
including intermediate recursive nodes. Paging and sentinel counts therefore
range over the filtered result. Namespace options and query limits are unchanged.

This compiler operates on one database. Generic graph endpoint resolution can
search all configured backends. If an entity exists only in another backend,
the single-database query can withhold an edge that a generic graph read permits.
This API does not add cross-backend query federation.

## `CompileOptions`

`scopes` supplies namespace filters. An empty vector means cross-namespace; otherwise every applicable table is filtered with bound parameters. Query text cannot supply this value.

`max_limit` is the effective per-call page size supplied by the runtime after applying its hard cap. An explicit query `LIMIT` bounds rows after `SKIP`: `SKIP 10 LIMIT 3` returns up to three rows beginning with the eleventh match. The payload bound is `min(LIMIT, max_limit)`, subject to checked `usize`-to-`i64` conversion. Without an explicit limit, the payload bound is `max_limit`. Query-verb callers normally set this through `page_size`; its public range is 1 through 10,000 rows.

`GqlQuery.offset` is emitted as a bound SQL `OFFSET`. It must fit SQLite's signed integer range. A negative GQL `SKIP` is rejected by the parser, and a hand-built or platform-sized value above `i64::MAX` is rejected before execution.

## Truncation sentinel

When an explicit `LIMIT` is at or below `max_limit`, the caller's limit is terminal and the compiler fetches at most that many rows, without a sentinel. This includes `LIMIT 0`, which returns an empty terminal page. When there is no explicit limit or the limit exceeds the page size, SQL fetches `max_limit + 1` rows and sets `CompiledQuery.truncation_check`. For example, `SKIP 90 LIMIT 100` with page size 50 fetches up to 51 rows after the offset, of which at most 50 are emitted. `SKIP 90 LIMIT 3` fetches at most three rows and no sentinel.

The execution site removes the sentinel and truncates to `TruncationCheck.max_limit`. `requested_limit` retains the caller's explicit value for diagnostics. Inspecting the extra row avoids both false warnings when a large limit matches few rows and silent truncation when an unbounded query matches more than the page size (issue #777). For GQL, a sentinel produces `has_more: true`, retains `truncated: true` for compatibility, and returns `next_offset = current offset + emitted rows`; terminal pages omit `next_offset`.

## Parameter and property binding

All scope values, relation filters, property values, depths, and limits are bound parameters. Integer values remain `INTEGER`, finite decimals remain `REAL`, booleans use integer `0`/`1`, and text equality uses `COLLATE NOCASE`. Non-finite floats in hand-built ASTs return `InvalidInput`.

Inline property equality continues to map arbitrary keys through `json_extract`. In `WHERE`, dedicated fields map directly to columns. Explicit node `properties.<path>` references extract from properties; canonical edge `metadata.<path>` references extract from metadata. Metadata paths and comparison values are bound parameters; existing node-property predicates retain their validated path literals. Unknown direct fields and bare containers are rejected; the same substrate-aware resolver is used by fixed- and variable-length compilation. Event predicates require a known event column. Existing direct edge predicates accept `relation` and `weight`, and edge JSON predicates require the metadata root, never properties. The `entity_type` field always uses its dedicated column. Literal wildcard characters are escaped before compiler-supplied `LIKE` wildcards are added.

`RETURN n.properties.<path>` and `RETURN e.metadata.<path>` validate every path segment and bind the complete JSON path as a text parameter. Node properties require a binding that exposes properties; metadata requires a canonical edge. Unknown bindings, empty/invalid paths, wrong binding kinds and `Path(Field(...))` in a hand-built AST are refused before execution. This does not change the existing WHERE node-path literal behavior.

Path extraction preserves SQLite values: integers and finite numbers stay numeric, strings stay text, booleans become `0`/`1`, and missing or JSON-null leaves become SQL null. Object and array leaves are JSON text; no flattening or general JSON expansion occurs. Output names join the variable, root and segments with underscores, preserving case. The compiler rejects different semantic expressions with the same exact name, including collisions with whole-binding fields or implicit traversal metrics, while repeated identical expressions retain their old behavior. For example, a recursive edge binding named `_total` cannot return its `weight` under the implicit `_total_weight` column name. Alias comparison uses actual fields, not display suffixes: a whole edge's `source_id` remains the field behind its `_source` output. Existing public-AST binding labels, including digits and quotes, remain quoted labels.

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

The compiler preserves edge direction, filters soft-deleted rows, applies namespace scope to every bound substrate, preserves `AND`/`OR` grouping, and resolves WHERE predicates and RETURN projections against the bound variable's substrate. Ordinary fixed-length results append every bound node and edge identity after any requested ordering terms, before applying `LIMIT`/`OFFSET`; substrate discriminators precede UUIDs so equal IDs in different union members remain distinct, and synthetic observations use their `(event_id, role, position)` primary key.

### Connected comma-separated paths

The fixed compiler walks all paths in textual order with globally unique node and edge aliases. Each later root adds a `CROSS JOIN` of the same filtered node source, followed by its ordinary edge/node joins. Shared node variables add both substrate and UUID equality predicates; disconnected components are refused before lowering. References retain the first alias, and every occurrence keeps its own filters. Namespace, deletion and mailbox predicates apply to every occurrence before matching and paging, with the caller actor parameter bound once and reused.

The result is the conjunction of path matches with bag multiplicity. Two matches on one branch and three on another yield six tuples. Distinct edge variables may select the same edge; ordinary projections deduplicate neither paths nor equal projected rows. Explicit DISTINCT and count grouping apply to the complete matched tuples, so each mandatory binding contributes six to a counts-only 2-by-3 match. There is one global WHERE tree, projection, order and paging tail. For ordinary projections, the identity suffix covers every occurrence in textual path/element order, with substrate before node UUID and edge UUID at each edge occurrence. Requested DESC terms do not reverse that suffix.

An empty additional-pattern vector preserves existing fixed, recursive, synthetic and SPARQL SQL and parameter bytes. Multi-pattern queries execute as one compiled read; no Rust-side joining or per-path paging is introduced. Connectedness does not bound intermediate fanout or sorting work. Page bounds remain result bounds, and cancellation and unchanged-data continuation qualifications from [ADR-008 Amendment 1](../../../../docs/adr/ADR-008-query-layer-separation.md#amendment-1-edge-metadata-and-bounded-gql-ordering-2026-11-14) continue to apply.

## Correlated absence lowering

`NotExists(pattern)` lowers to a correlated `NOT EXISTS (SELECT 1 FROM ... WHERE ...)` predicate. Each block receives fresh compiler-owned node/edge aliases. User binding names are map keys, never interpolated SQL identifiers. Every inner node uses the filtered primary-node union and every edge uses the filtered canonical-edge source; live-row and namespace predicates apply to every occurrence. The existing mailbox actor parameter is reused. Node kind, subtype and sorted inline-property constraints, edge directions and relation alternatives all remain part of existence matching.

Correlated nodes compare both ID and substrate. Ordinary outer nodes expose `substrate_kind`, synthetic observation targets expose `referent_kind`, and synthetic event sources use the event substrate. Local variables do not escape the block. The inner lookup has no result limit, count or projection deduplication. Multiple inner matches only change existence truth; they never multiply outer rows. Filtering runs before grouping, DISTINCT, ordering, offset and the sentinel.

On a single-variable-segment path, start-only blocks run in the seed phase. A block referencing the end runs in the final row; if it also references the start, lowering forces the typed final start join even when no start field is returned. Recursive absence uses the typed zero seed and both typed final joins for positive minima too, retaining the final positive-minimum filter. Same-phase OR stays grouped; cross-phase OR is refused, while independent AND conjuncts may route separately. Ordinary absence projections retain their implicit traversal metrics; explicit DISTINCT/count modes retain their existing metric-free result shape.

Queries without absence preserve their SQL and parameter sequence. The bounded direct-AST correction for `True OR x` and `x OR True` emits a true disjunction while retaining references to any lowered parameters; no general Boolean optimization is added.

## Caller ordering and continuation

`ORDER BY` resolves validated scalar fields or named node-properties/canonical-edge-metadata paths. It emits no extra result columns. Bare containers and namespace fields are rejected; direction is a typed `Asc` or `Desc`, never caller-supplied SQL. Queries without new path projections, counts or explicit distinct retain their existing SQL and parameters when their output names are unambiguous.

Sort values are not cast: ascending SQLite order is null, numeric, BINARY-collated text, then blob; descending reverses this value order. JSON array/object leaves remain JSON text. Predicate equality can still use NOCASE collation, so matching text case-insensitively does not imply case-insensitive sorting. Each requested direction applies only to its term; the existing identity tie suffix retains its direction.

All visibility and deletion predicates apply before ordering and the page sentinel. To continue when `has_more` is true, retain the query and order terms, replace `SKIP` with `next_offset`, and keep the page size. The query-text `LIMIT` applies again after each new offset; it does not impose a cumulative bound across requests. An explicit `LIMIT` at or below the page size produces a terminal response even when more underlying matches exist. Repeated reads of unchanged data are stable; concurrent changes can shift an offset page.

## DISTINCT and binding counts

Explicit `distinct` selects `DISTINCT` over requested values. A `Count(variable)` projection emits integer `COUNT(1)` for each mandatory fixed node/edge binding, including a synthetic observation row whose public edge ID is null. Recursive node counts also use `COUNT(1)`; recursive edge counts use `COUNT(t.via_edge)` so a null zero-hop edge contributes zero. Distinct paths that share a final edge still count separately. Names are `count_<variable>`, quoted and checked against canonical expression identities in the existing exact-case collision registry.

With counts present, `GROUP BY` contains every emitted nonaggregate expression, including all fields expanded from whole bindings. It contains no unreturned identities or traversal metrics. Counts-only queries omit GROUP BY, preserving SQLite's single zero-count row on empty input. Empty grouped input yields no rows. Explicit DISTINCT follows grouping; neither mode retains the implicit `_depth`/`_total_weight` columns.

Selected expressions, group keys and deterministic ties use explicit BINARY collation without text casts. Integers and real numbers keep numeric comparison, missing/JSON-null leaves group together, and text case remains significant. Actual SQL expressions and bound JSON-path parameters are reused instead of public output aliases, whose SQLite lookup is case-insensitive. Requested ORDER must derive from a returned field, exact path or whole JSON container; a count operand alone exposes no fields for ordering. Count functions and output labels do not become ORDER syntax.

Visibility, deletion predicates, absence checks and complete matching run before grouping. Ordering, SKIP and the unchanged post-SKIP limit/sentinel run on completed result groups or distinct values, never a pre-limited matching prefix. These are output bounds; they do not bound grouping or scan work.

## Synthetic observation edges

The four `observed_as_*` relations bind through `event_observations`, not `graph_edges`. Their source is an event; their target is the entity/note referent union, discriminated by `referent_kind` so equal UUID bytes on different substrates cannot collide (issue #468).

Legal role/target pairs are:

| Relation role           | Target substrate |
| ----------------------- | ---------------- |
| `candidate`, `selected` | note             |
| `target`, `signal`      | entity or note   |

Synthetic edges must be outbound and fixed-length. Mixing synthetic and canonical relations in one edge is rejected because the backing tables have no meaningful shared join key. Event nodes expose event columns and reject `entity_type` or arbitrary property maps; observation targets expose the common entity/note projection columns.

## Variable-length recursive CTE

The single-variable-segment route retains its existing `start -[*N..M]-> end` lowering. Paths with additional canonical steps use the mixed-path route below; synthetic variable segments remain unsupported.

For ordinary positive-minimum projections without absence, the recursive seed starts at depth one and retains its existing SQL and parameter order. For a zero minimum, the seed selects visible start nodes without joining an edge: depth is integer zero, total weight is real `0.0`, and every final-edge field is SQL null. An isolated node can therefore match. This branch carries both substrate and UUID for its start and current node, and final endpoint joins preserve that typed identity. Start and end constraints, including end constraints when the end is not returned, both apply to depth-zero rows.

The CTE binds the maximum depth and any positive minimum as parameters, records visited IDs in a path string to prevent cycles, filters deleted and out-of-scope intermediate nodes, and accumulates edge weight. A first expansion from depth zero may take a self-loop, preserving the existing depth-one seed behavior; subsequent expansions retain the same visited-ID guard. Namespace and mailbox filtering apply to the seed, every traversed node and edge, and the final endpoints. `0..0` never expands; `0..1` permits one expansion. Outgoing, incoming and either-direction traversal share these rules.

Because the ordinary recursive projection uses `DISTINCT`, its deterministic tie order is depth and descending total weight, then every projected output alias. That tuple is total over the rows that survive `DISTINCT` and avoids choosing an arbitrary hidden path as the representative for a collapsed row.

Requested order terms precede that tuple only if derivable from the actual returned projections. An exactly returned typed path can supply the same ordering reference. A returned JSON container permits ordering by a path within it; an unreturned field or a different hidden path does not. A returned scalar is not reinterpreted as a JSON document to expose further paths. Whole-edge projections omit metadata, and whole-node projections do not implicitly return description. Non-derivable terms return `Unsupported` containing `variable-length ORDER BY must be derived from RETURN projections`. The compiler does not add hidden fields to the DISTINCT projection to make an otherwise ambiguous order legal.

When a query returns a typed path, requested ordering, traversal-metric ordering and projection-value ties use the already selected SQL expressions, reusing bound path parameters. Container-derived ordering extracts from the selected container expression. This avoids SQLite's case-insensitive output-alias lookup without folding or refusing labels that differ only by case. Ordinary queries without path projections retain their previous alias-based SQL.

Canonical edge metadata describes the final traversed edge, including returned typed metadata paths. The CTE declaration, seed and step carry `via_metadata` only when RETURN, WHERE or ORDER BY needs it; old queries retain their exact CTE SQL and parameters. Final-edge metadata predicates run after traversal, alongside end-node predicates, so an earlier edge's metadata cannot incorrectly prune the path. At zero hops, the explicit metadata container and any extraction from it are null, without borrowing an incident edge's metadata.

Distinct/grouped recursive modes always use the typed zero-seed branch, even for positive minima; the final depth predicate excludes rows below that minimum. Both final endpoint joins include substrate equality, preventing same-UUID entity/note fanout from multiplying raw path counts. The existing first-expansion self-loop allowance and later revisit guard remain intact. Ordinary positive-minimum SQL and parameters remain unchanged.

The end-node union is always joined because end filters apply even when the end variable is not projected. A start-node join is emitted only when its columns are returned.

`OR` within one phase is preserved, including a final-edge metadata predicate combined with an end-node predicate. An `OR` spanning start-node and final-row phases is rejected because routing its halves to separate CTE phases would silently change it into `AND`; cross-phase `AND` remains supported.

## Mixed canonical paths

A single canonical path may contain fixed steps and several bounded variable segments. Each variable segment has its own typed recursive relation carrying start/current substrate and UUID, raw path rows, depth, weight and nullable final-edge fields. A zero-hop seed identifies one visible record without requiring an incident edge. Adjacent segment/fixed joins retain substrate and UUID, and every boundary node keeps its kind, subtype and inline constraints. The sum of variable maxima is at most ten; normalized `1..1` fixed steps are excluded.

Visited-node state resets for each segment. A later segment may revisit a record from an earlier segment, while each segment retains the existing first-expansion self-loop allowance and subsequent cycle guard. Every recursive intermediate, fixed endpoint and edge retains live-row, namespace and mailbox filtering. No segment receives an early LIMIT or projection deduplication.

Every named node is available to typed absence correlation. The full WHERE Boolean tree runs on the complete joined binding row, including OR between fixed and variable bindings, before grouping, DISTINCT, order and pagination. This does not change the legacy single-segment cross-phase OR refusal. A variable edge still accepts metadata-path predicates on its own final edge; fixed-edge predicates retain their existing field surface.

Ordinary `_depth` and `_total_weight` sum only variable-segment hops and weights. Fixed-step weights and hops are excluded. Each variable edge projection identifies that segment's final edge, not a later edge; fixed bindings preserve their own fields. `count(variable_edge)` excludes its null zero-hop final edge, while nodes and fixed edges count matched rows. Raw path multiplicity survives until grouping or final projection DISTINCT. Explicit DISTINCT/count modes omit implicit metrics and group only by returned values.

Deterministic returned-value ordering, SKIP, post-SKIP LIMIT and the existing sentinel apply to completed results. Output caps do not bound intermediate fanout. Existing fixed-only and single-variable SQL/parameter behavior remains unchanged.

## Projection columns and failure modes

Whole-variable projections expand to the allowed columns for their substrate. Direct property projections are checked against node, event, observation-target, or edge whitelists; unknown columns return `QueryError::Compile`. Typed JSON paths use the separate binding-aware, bound-path resolver. Fixed-length `RETURN e` retains `id`, `source_id`, `target_id`, `relation`, and `weight`; recursive `RETURN e` retains the final edge's `id`, `relation`, and `weight`. Metadata requires explicit `RETURN e.metadata`. Edge predicates accept direct `relation`/`weight` or explicit metadata paths. Synthetic observation edges reject metadata projection, predicates and ordering.

Common unsupported cases include repeated node variables within a path, repeated edge bindings, disconnected comma components, recursive or synthetic multi-pattern matches, synthetic steps in mixed variable paths, SPARQL `*`, variable-length synthetic edges, and `OR` spanning seed and final phases of a single variable segment. Limits or depths that cannot fit the SQL parameter type return `InvalidInput` rather than wrapping.
