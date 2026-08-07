# SQL Compilation

The SQL compiler lowers a validated `GqlQuery` into a read-only, parameterized `CompiledQuery`. Fixed-length patterns use JOIN chains; a single variable-length edge uses a recursive CTE with cycle prevention and deterministic result ordering.

## `compile`

`compile(query, options)` validates the AST, selects the fixed- or variable-length lowering path, and returns SQL, ordered parameters, return projections, warnings, and optional truncation metadata. It can return any `QueryError` raised by validation or lowering; it never executes the SQL.

As defense in depth, the final statement must begin with `SELECT` or `WITH`. Parser guards already reject writes, and the database reader is independently read-only; this compiler check prevents a future lowering path from emitting a mutation by mistake.

## `CompileOptions`

`scopes` supplies namespace filters. An empty vector means cross-namespace; otherwise every applicable table is filtered with bound parameters. Query text cannot supply this value.

`max_limit` is the effective per-call page size supplied by the runtime after applying its hard cap. The payload bound is the lesser of an explicit query `LIMIT` and `max_limit`, subject to checked `usize`-to-`i64` conversion. Query-verb callers normally set this through `page_size`; its public range is 1 through 10,000 rows.

`GqlQuery.offset` is emitted as a bound SQL `OFFSET`. It must fit SQLite's signed integer range. A negative GQL `SKIP` is rejected by the parser, and a hand-built or platform-sized value above `i64::MAX` is rejected before execution.

## Truncation sentinel

When an explicit `LIMIT` is at or below `max_limit`, the caller's limit is terminal and the compiler fetches exactly that number. When there is no explicit limit or it exceeds the page size, SQL fetches `max_limit + 1` rows and sets `CompiledQuery.truncation_check`.

The execution site removes the sentinel and truncates to `TruncationCheck.max_limit`. `requested_limit` retains the caller's explicit value for diagnostics. Inspecting the extra row avoids both false warnings when a large limit matches few rows and silent truncation when an unbounded query matches more than the page size (issue #777). For GQL, a sentinel produces `has_more: true`, retains `truncated: true` for compatibility, and returns `next_offset = current offset + emitted rows`; terminal pages omit `next_offset`.

## Parameter and property binding

All scope values, relation filters, property values, depths, and limits are bound parameters. Integer values remain `INTEGER`, finite decimals remain `REAL`, booleans use integer `0`/`1`, and text equality uses `COLLATE NOCASE`. Non-finite floats in hand-built ASTs return `InvalidInput`.

Inline property equality continues to map arbitrary keys through `json_extract`. In `WHERE`, dedicated fields such as `name` or `content` map directly to columns, while only explicit `properties.<path>` references map to `json_extract(alias.properties, '$.path')`. Unknown direct fields and the bare `properties` container are rejected; the same resolver is used by fixed- and variable-length compilation. Event predicates require a known event column, and edge predicates accept only `relation` and `weight`; neither accepts JSON-property paths. `entity_type` always uses its dedicated column. For `LIKE`-family operations, literal `%`, `_`, and `\` are escaped before compiler-supplied wildcards are added.

Node kind labels `entity`, `note`, `event`, and `edge` select a substrate in the primary-node union; granular values such as `concept`, `task`, or comm's pack-registered `message` filter the stored `kind`. No stored row is expected to have the literal granular kind `entity` (issue #849).

Granular kind filtering is intentionally data-driven. The compiler has no `VerbRegistry`
dependency and does not keep a second allowlist of pack-owned kinds: registration validates the
write, while GQL binds the stored kind as a SQL parameter. `MATCH (m:message)` and
`MATCH (m:note) WHERE m.kind = "message"` therefore select the same caller-visible message
notes. GQL's field is `kind`; `note_kind` is terminology from the verb parameter surface and is
not a query field. Runtime-supplied namespace scopes still apply to every union member.

## Fixed-length JOIN compilation

Canonical edges bind through `graph_edges`, while endpoint nodes bind through a union of entities, notes, events, and graph edges. This substrate-agnostic source is necessary because relations such as `annotates` can target several substrates and epistemic relations can connect notes (issue #467).

The compiler preserves edge direction, filters soft-deleted rows, applies namespace scope to every bound substrate, preserves `AND`/`OR` grouping, and resolves WHERE predicates and RETURN projections against the bound variable's substrate. Fixed-length results are ordered by every bound node and edge identity before applying `LIMIT`/`OFFSET`; substrate discriminators precede UUIDs so equal IDs in different union members remain distinct, and synthetic observations use their `(event_id, role, position)` primary key.

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

The recursive seed starts at depth one. The CTE binds maximum and minimum depth as parameters, records visited IDs in a path string to prevent cycles, filters deleted and out-of-scope intermediate nodes, and accumulates edge weight. Because the final projection uses `DISTINCT`, results order first by depth and descending total weight, then by every projected output alias. That tuple is total over the rows that survive `DISTINCT` and avoids choosing an arbitrary hidden path as the representative for a collapsed row.

The end-node union is always joined because end filters apply even when the end variable is not projected. A start-node join is emitted only when its columns are returned.

`OR` within one endpoint is preserved. An `OR` spanning start and end endpoints is rejected because routing its halves to separate CTE phases would silently change it into `AND`; cross-endpoint `AND` remains supported.

## Projection columns and failure modes

Whole-variable projections expand to the allowed columns for their substrate. Property projections are checked against node, event, observation-target, or edge whitelists; unknown columns return `QueryError::Compile`. Only `relation` and `weight` are valid edge properties in query predicates.

Common unsupported cases include repeated variables, zero-hop paths, mixed fixed/variable chains, SPARQL `*`, variable-length synthetic edges, and `OR` spanning both variable-length endpoints. Limits or depths that cannot fit the SQL parameter type return `InvalidInput` rather than wrapping.
