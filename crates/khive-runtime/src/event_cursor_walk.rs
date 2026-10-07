use khive_storage::event::{Event, EventFilter};
use khive_storage::types::PageRequest;
use khive_storage::StorageError;

use crate::events_split::MAX_QUERY_EVENTS_PAGE_ROWS as TRANSPORT_PAGE_ROWS;

/// Causes left to each caller to classify and describe for its own surface.
#[derive(Debug)]
pub enum EventCursorWalkError {
    Storage(StorageError),
    MissingBoundary,
    DenseTimestampTie { page_limit: u32 },
}

/// Visit at most `max_rows` owned events in the store's descending page order.
///
/// Re-read timestamp boundaries at offset zero and deduplicate their IDs; widen
/// pages only up to the event transport cap. A wider tie returns a typed cause.
/// Reads are independent against a live event plane, not a shared snapshot.
/// The callback can have received a prefix before a later read or dense tie fails.
pub async fn visit_events_cursor_walk<F: FnMut(Event)>(
    store: &dyn khive_storage::event::EventStore,
    base_filter: &EventFilter,
    page_size: u32,
    max_rows: u64,
    mut visit: F,
) -> Result<u64, EventCursorWalkError> {
    let mut admitted = 0;
    let mut cursor: Option<i64> = base_filter.before;
    let mut boundary_at: Option<i64> = None;
    let mut boundary_ids: std::collections::HashSet<uuid::Uuid> = std::collections::HashSet::new();
    let mut fetch_limit = page_size.clamp(1, TRANSPORT_PAGE_ROWS);
    while admitted < max_rows {
        let mut filter = base_filter.clone();
        filter.before = cursor;
        let page = store
            .query_events(
                filter,
                PageRequest {
                    offset: 0,
                    limit: fetch_limit,
                },
            )
            .await
            .map_err(EventCursorWalkError::Storage)?;
        let fetched = page.items.len() as u64;
        let fresh: Vec<Event> = page
            .items
            .into_iter()
            .filter(|event| !boundary_ids.contains(&event.id))
            .collect();
        if fresh.is_empty() {
            if fetched < u64::from(fetch_limit) {
                // The store returned everything under the cursor and all of
                // it was already collected: the window is exhausted.
                break;
            }
            // A full page of already-collected boundary rows: the tie run at
            // this microsecond fills the page. Widen and re-read — but only
            // up to the transport cap, past which the daemon refuses the
            // request.
            if fetch_limit >= TRANSPORT_PAGE_ROWS {
                // At the cap, distinguish a tie run that exactly fills the
                // page (fully collected, pageable by stepping the strict
                // bound to the boundary itself) from one wider than the cap
                // (genuinely unpageable with a timestamp cursor). Every
                // collected row is >= the boundary microsecond, so equality
                // of the at-or-above count with the collected count proves
                // the run is complete.
                let boundary = boundary_at.ok_or(EventCursorWalkError::MissingBoundary)?;
                let mut ge_boundary = base_filter.clone();
                // `after` is a strict `created_at >` bound, so at-or-above
                // the boundary is `> boundary - 1`. At `i64::MIN` every row
                // already satisfies at-or-above; keep the base bound.
                ge_boundary.after = boundary.checked_sub(1).or(base_filter.after);
                let ge_total = store
                    .count_events(ge_boundary)
                    .await
                    .map_err(EventCursorWalkError::Storage)?;
                if ge_total == admitted {
                    cursor = Some(boundary);
                    continue;
                }
                return Err(EventCursorWalkError::DenseTimestampTie {
                    page_limit: fetch_limit,
                });
            }
            fetch_limit = fetch_limit.saturating_mul(2).min(TRANSPORT_PAGE_ROWS);
            continue;
        }
        // Pages come back created_at DESC, so the last fresh row carries the
        // new boundary microsecond.
        let boundary = fresh
            .last()
            .map(|event| event.created_at)
            .expect("fresh is non-empty");
        if boundary_at != Some(boundary) {
            boundary_ids.clear();
            boundary_at = Some(boundary);
        }
        boundary_ids.extend(
            fresh
                .iter()
                .filter(|event| event.created_at == boundary)
                .map(|event| event.id),
        );
        // Preserve the old final truncate: only the remaining prefix is
        // admitted, so surplus payloads never reach the aggregate callback.
        let remaining = usize::try_from(max_rows - admitted).unwrap_or(usize::MAX);
        for event in fresh.into_iter().take(remaining) {
            visit(event);
            admitted += 1;
        }
        // `i64::MAX` admits no exclusive bound above it: keep the cursor as
        // is and re-read — dedup drops the re-admitted rows, and the
        // at-the-cap completeness check above advances past the boundary (or
        // reports the dense tie) once a page comes back all-duplicates.
        cursor = if boundary == i64::MAX {
            cursor
        } else {
            Some(boundary + 1)
        };
        if fetched < u64::from(fetch_limit) {
            break;
        }
    }
    Ok(admitted)
}
