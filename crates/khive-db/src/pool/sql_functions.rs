//! Application-defined read helpers, registered on every pooled connection.

use rusqlite::Connection;

use crate::error::SqliteError;

pub(super) fn register_writer_clock(conn: &Connection) -> Result<(), SqliteError> {
    // Evaluated by SQLite at statement execution, never deterministic: stream
    // observation deadlines use the same UTC microsecond source as note stamps.
    conn.create_scalar_function(
        "khive_now_micros",
        0,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8,
        |_| Ok(chrono::Utc::now().timestamp_micros()),
    )?;
    Ok(())
}

/// Order-preserving UTC key across Chrono's signed timestamp range, with
/// nanoseconds kept after the sign-adjusted epoch seconds.
pub(crate) fn rfc3339_instant_key(instant: chrono::DateTime<chrono::Utc>) -> Vec<u8> {
    let mut key = Vec::with_capacity(12);
    key.extend_from_slice(&((instant.timestamp() as u64) ^ (1_u64 << 63)).to_be_bytes());
    key.extend_from_slice(&instant.timestamp_subsec_nanos().to_be_bytes());
    key
}

/// The outbox deadline grammar is stricter than the general timestamp filter.
/// This is shared by app-maintained stored keys, V44 backfill, and the read
/// residual; no schema expression calls an application-defined function.
pub(crate) fn strict_rfc3339_key(text: &str) -> Option<Vec<u8>> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|instant| rfc3339_instant_key(instant.with_timezone(&chrono::Utc)))
}

/// Register timestamp-key functions for read filters on pooled connections.
pub(crate) fn register_rfc3339_key(conn: &Connection) -> rusqlite::Result<()> {
    use rusqlite::functions::FunctionFlags;
    use rusqlite::types::ValueRef;

    conn.create_scalar_function(
        "khive_rfc3339_key",
        1,
        FunctionFlags::SQLITE_UTF8
            | FunctionFlags::SQLITE_DETERMINISTIC
            | FunctionFlags::SQLITE_INNOCUOUS,
        |ctx| {
            let text = match ctx.get_raw(0) {
                ValueRef::Text(bytes) => std::str::from_utf8(bytes).ok(),
                _ => None,
            };
            let key = text
                .and_then(|text| text.parse::<chrono::DateTime<chrono::Utc>>().ok())
                .map(rfc3339_instant_key);
            Ok(key)
        },
    )?;
    // The outbox's legacy retry predicate used parse_from_rfc3339, while the
    // general key above accepts Chrono's relaxed DateTime FromStr grammar.
    // Keep the strict grammar separate so a relaxed-only future value still
    // fails open as malformed, instead of postponing the message forever.
    conn.create_scalar_function(
        "khive_rfc3339_strict_key",
        1,
        FunctionFlags::SQLITE_UTF8
            | FunctionFlags::SQLITE_DETERMINISTIC
            | FunctionFlags::SQLITE_INNOCUOUS,
        |ctx| {
            let text = match ctx.get_raw(0) {
                ValueRef::Text(bytes) => std::str::from_utf8(bytes).ok(),
                _ => None,
            };
            let key = text.and_then(strict_rfc3339_key);
            Ok(key)
        },
    )?;
    Ok(())
}

/// Keep reader, writer-slot, and standalone writer functions identical.
pub(super) fn register_read_functions(conn: &Connection) -> rusqlite::Result<()> {
    register_rfc3339_key(conn)?;
    conn.create_scalar_function(
        "khive_tag_contains",
        2,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8
            | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC
            | rusqlite::functions::FunctionFlags::SQLITE_INNOCUOUS,
        |ctx| {
            let tags = match ctx.get_raw(0) {
                rusqlite::types::ValueRef::Text(bytes) => Some(String::from_utf8_lossy(bytes)),
                _ => None,
            };
            // Non-text tags match the pack row decoder's empty-text fallback.
            // A non-text marker cannot name a tag.
            let marker = match ctx.get_raw(1) {
                rusqlite::types::ValueRef::Text(bytes) => Some(String::from_utf8_lossy(bytes)),
                _ => None,
            };
            Ok(marker.is_some_and(|marker| {
                khive_types::tag_contains(tags.as_deref().unwrap_or(""), &marker)
            }))
        },
    )
}

#[cfg(test)]
mod tests;
