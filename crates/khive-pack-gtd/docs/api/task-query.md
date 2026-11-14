# Bounded task scanning (`src/handlers.rs`)

`gtd.next` and `gtd.tasks` need to filter a potentially large `task` note
population by `property_filters` (status, assignee, priority) without either
scanning the entire table on every call or missing older matching tasks
behind a wall of newer non-matching churn.

## Legacy state filtering (#2394)

Default `gtd.tasks` selects canonical open status strings before pagination;
terminal and unrecognized strings such as `archived` do not consume page slots.
Missing and non-text statuses retain the legacy `inbox` fallback. Explicit
canonical status filters and caller aliases are unchanged.

When matching records were excluded and the default page is empty, the existing
object envelope lists `done`, `cancelled`, and `unrecognized_status` under
`filter_excluded`. Inspect historical rows with `list(kind="task")` or by-ID `get`;
the query never rewrites their status, history, or timestamps. Unknown-state
blockers are reported as `invalid` with a `broken` dependency state.

`gtd.transition` and `gtd.complete`, including atomic preparation, reject unknown
stored string states with an explicit invalid-stored-status error. Repair needs
explicit source evidence, preserving `archived_at` and transition history.
[`gtd.repair`](task-repair.md) previews caller-specified replacements and applies
them only when the supplied observed values still match. It infers no timestamp
units or replacement dates. The separate read-only
[`gtd.census`](task-timestamp-census.md) reports raw value magnitudes; it neither
changes these status filters nor claims the historical timestamp units. There
is no `gtd.stats` verb.

## `gtd.tasks` page continuation (#6549)

`gtd.tasks` defaults to 50 tasks per page and clamps a supplied limit to 1–200.
It applies the complete task filter and namespace visibility before pagination,
in stable `created_at DESC, id ASC` order. The page query requests one additional
matching row, at most 201 rows, without computing a total.

When that extra row exists, the result is an object with `tasks` containing only
the requested page and `has_more: true`. An incomplete page that previously
returned a bare array now returns this object. Callers that index the result as
an array must read `tasks` for an incomplete page. The extra row is removed
before dependency diagnostics, so it is neither returned nor annotated.

When there is no extra row, the existing response shape is preserved: an
ordinary complete page is an array, including an exactly full final page; a
complete empty page is `[]` unless the existing excluded-status explanation
applies. A requested limit above 200 still produces an object with `tasks`,
`requested_limit`, `effective_limit`, and `limit_clamped: true`, even when no
further task exists. If both conditions apply, the object also has
`has_more: true`. A clamped limit does not by itself establish continuation.
No `has_more: false` field is added to an exhausted page.

Continue with the same filters and `offset` increased by the number of returned
tasks. For example, a default page of 50 out of 51 matching tasks has
`has_more: true`; the next call with `offset=50` returns the remaining task as a
complete array. An exactly full page of 50 out of 50 remains an array. There is
no new cursor or total-count field.

Consumers must accept either a bare array or an object's `tasks` array.
Continue when `has_more` is true; the presence of an object or a full page
alone does not establish continuation. An older short-page loop remains
valid only if it accepts both shapes and compares the row count with the
effective clamped limit, not the requested limit. It can require an extra
empty request after an exactly full final page.

For a known nonempty object page, a chained read can select
`$prev.tasks[0].id`; a nonempty bare array uses `$prev[0].id`. When the shape
or presence of a row is unknown, inspect the page before issuing a separate
call with the chosen identifier. Indexing an object directly with `$prev[0]`
produces a `substitution_error` with reason `path_wrong_type`; see the
[exact diagnostic and source references](../../../../docs/adr/ADR-019-gtd-pack.md#amendment-8-proposed-2026-11-13-disclose-task-page-continuation-6549).

Continuation describes matching live task rows, including blocked or broken
tasks. Dependency diagnostics annotate these rows; they do not remove tasks
from the page. The separate `filter_excluded` probe checks for an excluded
status at offset zero under the same other predicates when the default page is
empty. It remains an existence explanation, not a continuation signal or total.

The lookahead and returned rows come from one page-query snapshot. Dependency
diagnostics use subsequent reads, and successive offset pages do not share a
snapshot: concurrent inserts or deletions can shift page boundaries. One extra
returned row does not bound the work needed to evaluate filters or an offset.
JSON and table formats preserve the continuation marker.

This continuation contract does not apply to `gtd.next`. That verb selects
at most its effective limit by priority and truncates further candidates
without a `has_more` marker. Its limit-clamp report is not a completeness
signal. Use paged `gtd.tasks` for inventory, with its different ordering and
dependency diagnostics.

## `fetch_all_matching_tasks` — bounded single-snapshot scan (issue #772, #825)

Fetches every `task` note matching `property_filters` in a single bounded
snapshot query, rather than pre-fetching a fixed-size unfiltered window and
filtering client-side. The predicate is pushed into SQL via
`query_notes_filtered_bounded`, so the candidate set returned is bounded by
how many tasks actually **match** the filter — not by how many task notes of
any status exist.

This fixes a real starvation bug (issue #772): a fixed-size unfiltered
window could be entirely filled by newer non-matching churn (e.g. recently
created `inbox` tasks), hiding older matching tasks (e.g. long-standing
`next` tasks) regardless of their priority — the caller would never see them
even though they were exactly what the filter asked for.

### `query_notes_filtered_bounded` — one snapshot, not paged reads

Fetches at most `TASK_SCAN_MAX_ROWS + 1` rows in **one** SQL statement with
deterministic ordering — a single consistent snapshot, not a `COUNT(*)`
followed by independent paged reads. The prior page-loop implementation
re-queried the store per page with no transaction spanning them (issue
#825): a row inserted between pages could appear duplicated across a page
boundary, or the scan could hit its row cap and still return `Ok` with a
silently incomplete result set.

If `TASK_SCAN_MAX_ROWS + 1` rows come back, the function returns
`Err(InvalidInput)` instead of ever returning a possibly-truncated result —
callers must narrow the filters (e.g. add an `assignee` filter) so the
result set stays complete rather than silently dropping matching tasks past
the row cap.

## Dependency diagnostics

Dependency scheduling reads `properties.depends_on`. To change blockers, use
`update(id="<dependent task UUID>", properties={"depends_on":["<blocker task UUID>"]})`;
an empty array clears the scheduling dependencies. Targets must be live task notes
in the dependent task's namespace, and self-dependencies and cycles are refused.

Every task returned by `gtd.tasks` carries `dependency_state`, `actionable`,
and `blocked_by`. A dependency state is `ready` when every blocker is done,
`blocked` when at least one live blocker is still pending, and `broken` when a
blocker is cancelled, soft-deleted, missing after hard deletion, malformed, or
no longer a compatible task. Each `blocked_by` entry names the blocker id and
its structural state, so a task does not remain an unexplained plain `next`
record after its blocker becomes terminal or disappears.

These three fields are derived output. Generic task updates reject incoming
`properties.blocked_by`, `properties.dependency_state`, and `properties.actionable`,
including null, before any part of the patch writes. Unrelated updates to legacy
records containing those properties remain supported; the query continues deriving
its diagnostics and does not clean up historical properties.

With KG and GTD loaded, a task-to-task
`link(source_id="<dependent task UUID>", target_id="<blocker task UUID>", relation="depends_on")`
is supported. That graph link is traversable but does not set the scheduling
property. Generic `properties.depends_on` updates do not synchronize graph edges
either. Task creation stores dependencies and attempts a best-effort edge
projection. Completing a blocker changes readiness without rewriting the dependent
task's lifecycle or deleting its existing edges.

Graph-link cycle validation pages outgoing `depends_on` edges through the shared
GraphStore, then reads their endpoints through NoteStore. Only edges between live
task notes participate. The edge namespace is the caller's namespace; endpoint
lookup uses the stored task IDs without an additional namespace restriction.
Proposed links in the same batch participate before any link is written.

The breadth-first walk retains a 20,000-node and 20,000-admitted-edge safety
budget and uses a 20,000-hop depth bound. A separate 20,000-row scan budget also
counts candidate edges whose endpoints are not live task notes. A budget refusal
or typed storage decode error rejects the proposed link; a partial walk never
establishes that a link is acyclic. This scan budget is
stricter than the previous task-filtered SQL query for graphs with many excluded
endpoints. Stored targets are visited in UUID order, independently of edge weight.

`gtd.next` remains actionable-only by default. Set `include_blocked=true` to
include blocked and broken `next`/`active` tasks in the same array; ready work
sorts first, and every returned task's `actionable` field distinguishes work
that can be scheduled from dependency diagnostics.

The query path batches live blocker reads and probes only unresolved ids for a
soft-deleted row. A hard-deleted blocker therefore classifies as `missing`,
while a tombstone classifies as `soft_deleted` without resurrecting it.

## Proposed additive `gtd.tasks` filters (#2678)

This section implements the proposed ADR-019 Amendment 3; its contract approval
is pending. `gtd.tasks` additionally accepts:

- `tags`: array of strings, using SQLite `NOCASE` tag membership (ASCII
  case-insensitive, not general Unicode case folding). Missing, null, or empty
  tags add no restriction. Duplicate tags do not duplicate rows.
- `tag_mode`: `any` (default, also for null) or `all`. Any-of requires at least
  one requested tag; all-of requires every requested tag. With no tags, either
  mode adds no restriction. Invalid modes or malformed tag arrays are refused.
- `context_entity_id`: a complete UUID spelling accepted by the UUID parser,
  canonicalized to lowercase dashed form and compared with the stored task
  property. Missing/null adds no restriction. Short prefixes, malformed UUIDs,
  and non-string values are refused. This is a stored-reference comparison,
  with no entity lookup: a retained reference can match after its anchor is
  deleted, and an unused UUID yields no matches. Historical noncanonical stored
  strings are not rewritten or treated as canonical equivalents.

These filters combine with namespace, status, assignee, and priority using AND.
Explicit namespace routing remains precise; default reads keep the configured
visible task namespaces. The context parameter does not impose an additional
primary-namespace entity check.

Both tags and context become storage predicates before `PageRequest` limit and
offset. Newer nonmatching tasks cannot consume a filtered page. The one-row
excluded-status probe clones the same complete filter and changes only status,
so unrelated terminal tasks cannot produce an exclusion hint. No caller-defined
SQL or JSON path is accepted.

Omitting these filters preserves the default limit of 50, limits clamped to
1–200, and stable `created_at DESC, id ASC` order. The
[page-continuation contract](#gtdtasks-page-continuation-6549) applies to filtered
and unfiltered pages. The existing empty `tasks`/`filter_excluded` explanation
still requires a matching excluded task. Dependency diagnostics and timestamps
are unchanged.

## Duplicate judgments

`gtd.tasks(duplicate_of="<kept task>")` returns reverse references from
`properties.duplicate_of`. This new filter defaults to cancelled tasks; an
explicit status overrides that default. Ordinary visibility, filters and
pagination still apply. See [duplicate cancellation](duplicate-cancellation.md)
for partner validation, no-op semantics and retained-reference reads.
