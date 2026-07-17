# Predicate-pushdown scan cliff (issue #225)

Technical reference for the `search` handler's candidate-window sizing when property/tag
predicates are active, and why an unfiltered result-count `limit` alone under-sizes the
underlying FTS scan.

## `FILTERED_SCAN_CAP` (`handlers/search.rs`)

Maximum candidate window used when property/tag filters are active. Predicates are applied
BEFORE result truncation, inside the runtime's candidate budget (`search_limit ×
CANDIDATE_MULTIPLIER`). This constant widens the handler's initial `search_limit` so that
sparse matches ranked just below the bare `limit` remain within the candidate window.
Matches ranked beyond the overall budget may still be missed — use specific query text to
keep target records near the top of the ranking.

Both the entity and note search branches widen the candidate window the same way. Predicates
are applied before result truncation: entity tags are filtered SQL-level via `EntityFilter`;
entity/note properties and note tags are filtered Rust-level in the alive-set loop, since
notes have no dedicated tag column (tags live in `properties["tags"]`). The cap bounds
worst-case scan cost.

## The cliff mechanics

With `limit=1` the handler sets `search_limit = (1 * 50).min(500) = 50`, so only 50
candidates enter the runtime. The runtime then widens to `search_limit * CANDIDATE_MULTIPLIER
(4) = 200` candidates. Without predicate pushdown into `hybrid_search`/`search_notes`, a
target ranked below the handler's initial 50-record scan was invisible even though it was
within the runtime's 200-candidate budget; with the fix, filters are applied BEFORE
truncation over the full 200-candidate set.

Budget constants (`handlers/search.rs` and `retrieval.rs`):

| Stage                        | Formula                          | Result (limit=1) |
| ----------------------------- | --------------------------------- | ------------------ |
| Handler `search_limit`        | `(limit * 50).min(500)`           | 50                |
| Runtime candidates            | `search_limit * CANDIDATE_MULTIPLIER (4)` | 200        |
| Old cliff (pre-fix)           | beyond handler's 50-record scan    | rank 51           |
| New cliff (post-fix)          | beyond runtime's 200-candidate budget | rank 201      |

## Regression coverage

Four handler-level regressions (`handler_search_{entity,note}_{tag,props}_filter_beyond_scan_cliff`
in `dispatch.rs`) each insert 51 non-matching decoys ranked above the target in FTS, with
the target — carrying the required tag/property — sitting at rank 52 (just past the old
50-record cliff, still inside the new 200-candidate budget). A fifth test,
`handler_search_note_sanitizes_fts5_metacharacters`, is a plain FTS5 metacharacter
sanitization regression with no cliff mechanics.
