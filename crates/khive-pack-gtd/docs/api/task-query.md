# Bounded task scanning (`src/handlers.rs`)

`gtd.next` and `gtd.tasks` need to filter a potentially large `task` note
population by `property_filters` (status, assignee, priority) without either
scanning the entire table on every call or missing older matching tasks
behind a wall of newer non-matching churn.

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
