# SQL Compilation

The SQL compiler lowers a validated `GqlQuery` into a read-only, parameterized `CompiledQuery`. Fixed-length patterns use JOIN chains; a single variable-length edge uses a recursive CTE with cycle prevention and deterministic result ordering.

## `compile`

`compile(query, options)` validates the AST, selects the fixed- or variable-length lowering path, and returns SQL, ordered parameters, return projections, warnings, and optional truncation metadata. It can return any `QueryError` raised by validation or lowering; it never executes the SQL.

As defense in depth, the final statement must begin with `SELECT` or `WITH`. Parser guards already reject writes, and the database reader is independently read-only; this compiler check prevents a future lowering path from emitting a mutation by mistake.

## `CompileOptions`

`scopes` supplies namespace filters. An empty vector means cross-namespace; otherwise every applicable table is filtered with bound parameters. Query text cannot supply this value.

`max_limit` is the server-side row cap. The effective limit is the lesser of the explicit query limit and the cap, subject to checked `usize`-to-`i64` conversion.

## Truncation sentinel

When an explicit `LIMIT` is at or below `max_limit`, the cap is not binding and the compiler fetches exactly that number. When there is no explicit limit or it exceeds the cap, SQL fetches `max_limit + 1` rows and sets `CompiledQuery.truncation_check`.

The execution site removes the sentinel and truncates to `TruncationCheck.max_limit`. `requested_limit` retains the caller's explicit value for diagnostics. Inspecting the extra row avoids both false warnings when a large limit matches few rows and silent truncation when an unbounded query matches more than the cap (issue #777).

## Parameter and property binding

All scope values, relation filters, property values, depths, and limits are bound parameters. Integer values remain `INTEGER`, finite decimals remain `REAL`, booleans use integer `0`/`1`, and text equality uses `COLLATE NOCASE`. Non-finite floats in hand-built ASTs return `InvalidInput`.

Inline property equality maps dedicated fields such as `name` or `content` to their columns and other keys to `json_extract(alias.properties, '$.key')`. `WHERE` predicates instead require a known column for the bound substrate; unknown names are rejected with the valid list. `entity_type` always uses its dedicated column. For `LIKE`-family operations, literal `%`, `_`, and `\` are escaped before compiler-supplied wildcards are added.

Node kind labels `entity`, `note`, `event`, and `edge` select a substrate in the primary-node union; granular values such as `concept` or `task` filter the stored `kind`. No stored row is expected to have the literal granular kind `entity` (issue #849).

## Fixed-length JOIN compilation

Canonical edges bind through `graph_edges`, while endpoint nodes bind through a union of entities, notes, events, and graph edges. This substrate-agnostic source is necessary because relations such as `annotates` can target several substrates and epistemic relations can connect notes (issue #467).

The compiler preserves edge direction, filters soft-deleted rows, applies namespace scope to every bound substrate, preserves `AND`/`OR` grouping, and validates WHERE predicates and RETURN projections against the bound variable's column whitelist.

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

The recursive seed starts at depth one. The CTE binds maximum and minimum depth as parameters, records visited IDs in a path string to prevent cycles, filters deleted and out-of-scope intermediate nodes, accumulates edge weight, and orders by depth, descending total weight, start ID, then current ID.

The end-node union is always joined because end filters apply even when the end variable is not projected. A start-node join is emitted only when its columns are returned.

`OR` within one endpoint is preserved. An `OR` spanning start and end endpoints is rejected because routing its halves to separate CTE phases would silently change it into `AND`; cross-endpoint `AND` remains supported.

## Projection columns and failure modes

Whole-variable projections expand to the allowed columns for their substrate. Property projections are checked against node, event, observation-target, or edge whitelists; unknown columns return `QueryError::Compile`. Only `relation` and `weight` are valid edge properties in query predicates.

Common unsupported cases include repeated variables, zero-hop paths, mixed fixed/variable chains, SPARQL `*`, variable-length synthetic edges, and `OR` spanning both variable-length endpoints. Limits or depths that cannot fit the SQL parameter type return `InvalidInput` rather than wrapping.
