use std::collections::BTreeMap;

use rusqlite::{Connection, Transaction};

use crate::error::SqliteError;

const PRE_V46_CAPTURE: &str = include_str!("../../sql/memory-visibility-pre-v46-capture.sql");

pub(super) fn capture_pre_v46(tx: &Transaction<'_>) -> Result<(), SqliteError> {
    tx.execute_batch(PRE_V46_CAPTURE)?;
    Ok(())
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct CutoverCounts {
    pub(super) legacy: u64,
    pub(super) modern: u64,
    pub(super) unknown: u64,
    pub(super) unknown_by_namespace: BTreeMap<String, u64>,
}

pub(super) fn cutover_counts(tx: &Transaction<'_>) -> Result<CutoverCounts, SqliteError> {
    let mut stmt = tx.prepare(
        "SELECT e.epoch, e.namespace, COUNT(*) FROM memory_visibility_epochs e \
         JOIN notes n ON n.id = e.note_id AND n.namespace = e.namespace \
         WHERE n.kind = 'memory' AND n.key IS NOT NULL \
         GROUP BY e.epoch, e.namespace ORDER BY e.epoch, e.namespace",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, u64>(2)?,
        ))
    })?;
    let mut counts = CutoverCounts::default();
    for row in rows {
        let (epoch, namespace, count) = row?;
        match epoch.as_str() {
            "legacy" => counts.legacy += count,
            "modern" => counts.modern += count,
            "unknown" => {
                counts.unknown += count;
                counts.unknown_by_namespace.insert(namespace, count);
            }
            _ => {
                return Err(SqliteError::InvalidData(
                    "invalid memory visibility epoch".into(),
                ))
            }
        }
    }
    Ok(counts)
}

pub(super) fn log_counts(counts: &CutoverCounts, database: &str) {
    tracing::info!(
        database,
        legacy = counts.legacy,
        modern = counts.modern,
        unknown = counts.unknown,
        unknown_by_namespace = ?counts.unknown_by_namespace,
        "memory visibility provenance cutover complete"
    );
}

pub(super) fn validate_cutover(conn: &Connection) -> Result<(), SqliteError> {
    // Prepare and step the real tables, so a forged version row cannot turn a
    // missing or unreadable provenance store into a successfully prepared backend.
    let mut stmt = conn.prepare(
        "SELECT e.note_id, e.namespace, e.epoch, p.note_id \
         FROM memory_visibility_epochs e \
         LEFT JOIN memory_visibility_pre_v46 p ON p.note_id = e.note_id LIMIT 0",
    )?;
    let mut rows = stmt.query([])?;
    let _ = rows.next()?;
    Ok(())
}

#[cfg(test)]
pub(super) mod test_state {
    use std::cell::Cell;

    use super::SqliteError;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Stop {
        AfterCapture,
        AfterV46Commit,
        BeforeCutoverCommit,
    }

    std::thread_local! {
        pub(crate) static STOP: Cell<Option<Stop>> = const { Cell::new(None) };
    }

    pub(crate) fn stop_at(point: Stop) -> Result<(), SqliteError> {
        let stop = STOP.with(|stop| {
            if stop.get() == Some(point) {
                stop.set(None);
                true
            } else {
                false
            }
        });
        if stop {
            Err(SqliteError::InvalidData(format!(
                "injected memory visibility stop: {point:?}"
            )))
        } else {
            Ok(())
        }
    }
}
