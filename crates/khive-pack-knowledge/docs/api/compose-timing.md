# `knowledge.compose` per-stage timing (`ComposeTiming`)

Source: `crates/khive-pack-knowledge/src/knowledge/compose.rs` (`pub(super)`, internal to
the crate).

## Call-ordering contract

`begin(phase)` must be called *before* that phase's work starts (in particular, before any
`.await` the phase covers) — it closes out whichever phase was previously active
(accumulating `last..now` into it) and opens `phase` as the new active phase. Because the
active phase is known at every instant, `finish` and `Drop` can both flush an in-flight
phase's partial duration into the breakdown rather than silently omitting it, which is
what a slow-then-failing or cancelled-mid-phase request needs.

`finish` must be the last thing called on every return path that completes the request
(success or a business-logic error) — in particular, after the response `Value` is fully
constructed, not before. Response-JSON assembly is real work and belongs inside whichever
phase is still active, typically `Trim`. `finish` flushes the active phase, flags the
timing as complete, and, if the total reaches `COMPOSE_SLOW_THRESHOLD_MS`, emits the
slow-request WARN.

If `finish` is never reached — because the enclosing future was dropped mid-poll (client
disconnect, cancellation, or daemon shutdown drain) — `Drop` performs the same flush and
emits a distinct "abandoned" WARN, so a request that never produces a response is not
silently invisible.

## Why `query_bytes` is stored eagerly

Records the query's UTF-8 *byte* length, not a char count — `str::len()` reads a value the
string already carries (O(1)), unlike `.chars().count()`'s O(n) UTF-8 walk. Because it is
O(1), there is nothing to gain by deferring it to the rare emission path as one would for
a genuinely O(n) computation: storing it eagerly costs the same as storing it lazily, and
eager storage avoids holding a borrow of the caller's query string for the tracker's
entire lifetime; `compose()` moves `raw_query` into the response body before calling
`finish()`, so a borrowing field would not compile.

## Why `finish` returns the per-phase breakdown

Production callers (`compose()`) discard the return value — the slow-request WARN above is
the real delivery mechanism there. The return value exists so tests can assert the *actual*
`finish()` call site flushed the still-active phase, without needing the request to run
long enough to cross `COMPOSE_SLOW_THRESHOLD_MS` and trigger the WARN itself. Earlier
regression tests called the private `flush_active` manually before `finish`/`drop`, so they
never exercised the flush call inside `finish`/`Drop` itself.
