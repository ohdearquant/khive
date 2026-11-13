//! Store schema work a serving process runs before it reports ready (ADR-154 Amendment 3).

use std::sync::atomic::Ordering;

use super::{StorageBackend, StoreSchemaKind};
use crate::error::SqliteError;
use crate::stores::index_repair::FORCED_INDEXES;
use crate::stores::note;

const MAPPED_TEXT_TABLES: &str = include_str!("../../sql/mapped-text-tables.sql");
const FORCED_INDEX_EXISTS: &str = include_str!("../../sql/forced-index-exists.sql");

const SERVING_STORE_KINDS: [StoreSchemaKind; 5] = [
    StoreSchemaKind::Entities,
    StoreSchemaKind::Notes,
    StoreSchemaKind::Graph,
    StoreSchemaKind::Agents,
    StoreSchemaKind::Events,
];

/// The result of [`StorageBackend::check_serving_forced_indexes`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ForcedIndexCheck {
    /// How many forced indexes were checked.
    pub checked: usize,
    /// The forced indexes that were missing and have been rebuilt, in check order.
    pub rebuilt: Vec<&'static str>,
}

impl StorageBackend {
    /// Run the schema ensure of every serving store kind (entities, notes,
    /// graph, agents, events), each in its own write unit.
    ///
    /// An ensure that already succeeded on this backend is not run again, so
    /// a store accessor called after this takes no writer for its schema. The
    /// `notes_seq` repair is separate: [`Self::repair_notes_seq_once`]. A
    /// read-only backend runs no schema work.
    pub fn prepare_serving_store_schemas(&self) -> Result<(), SqliteError> {
        for kind in SERVING_STORE_KINDS {
            self.ensure_store_schema(kind)?;
        }
        Ok(())
    }

    /// Run the `notes_seq` anti-join repair, which scans `notes` in full, if it
    /// has not yet run on this backend.
    ///
    /// Returns the number of `notes_seq` rows it inserted, or `None` when it
    /// had already run (here or on a notes store acquisition) or the backend
    /// is read-only. The notes schema ensure runs first under the same writer.
    pub fn repair_notes_seq_once(&self) -> Result<Option<usize>, SqliteError> {
        if self.is_read_only() || self.notes_seq_repair_runs.load(Ordering::Relaxed) != 0 {
            return Ok(None);
        }
        let writer = self.constructor_writer()?;
        self.ensure_store_schema_with_writer(StoreSchemaKind::Notes, writer.conn())?;
        self.repair_notes_seq_with_writer(writer.conn())
    }

    /// Run the repair once per backend; the caller holds the writer, which
    /// makes the check and the count update one step.
    pub(super) fn repair_notes_seq_with_writer(
        &self,
        conn: &rusqlite::Connection,
    ) -> Result<Option<usize>, SqliteError> {
        if self.notes_seq_repair_runs.load(Ordering::Relaxed) != 0 {
            return Ok(None);
        }
        let inserted = note::repair_notes_seq(conn)?;
        self.notes_seq_repair_runs.fetch_add(1, Ordering::Relaxed);
        Ok(Some(inserted))
    }

    /// The table keys of every FTS table in this store that has a rowid-map
    /// sidecar, in name order (`fts_<key>` gives `<key>`).
    ///
    /// FTS tables kept in step by triggers have no sidecar and are not listed.
    /// Passing each key to [`Self::text_with_tokenizer`] backfills its map, or
    /// writes the completion marker on an empty table.
    pub fn mapped_text_table_keys(&self) -> Result<Vec<String>, SqliteError> {
        let reader = self.pool.reader()?;
        let mut statement = reader.conn().prepare(MAPPED_TEXT_TABLES)?;
        let keys = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(keys)
    }

    /// Check that every index a typed read forces with `INDEXED BY` exists,
    /// and rebuild a missing one through its store's schema ensure.
    ///
    /// A read after readiness that has to repair a forced index then means the
    /// index was dropped while serving. A read-only backend cannot rebuild,
    /// so a missing index there is an error naming it, as is one still missing
    /// after its ensure ran.
    pub fn check_serving_forced_indexes(&self) -> Result<ForcedIndexCheck, SqliteError> {
        let missing = self.missing_forced_indexes()?;
        if missing.is_empty() {
            return Ok(ForcedIndexCheck {
                checked: FORCED_INDEXES.len(),
                rebuilt: Vec::new(),
            });
        }
        if self.is_read_only() {
            return Err(SqliteError::InvalidData(format!(
                "read-only database is missing forced index(es) {}; rebuild them in a \
                 writable copy before opening the snapshot",
                missing.join(", ")
            )));
        }
        {
            let writer = self.constructor_writer()?;
            for kind in SERVING_STORE_KINDS {
                if FORCED_INDEXES
                    .iter()
                    .any(|index| index.kind == kind && missing.contains(&index.name))
                {
                    self.store_schemas[kind as usize]
                        .ready
                        .store(false, Ordering::Release);
                    self.ensure_store_schema_with_writer(kind, writer.conn())?;
                }
            }
        }
        let still_missing = self.missing_forced_indexes()?;
        if !still_missing.is_empty() {
            return Err(SqliteError::InvalidData(format!(
                "forced index(es) {} still missing after their store schema ensure ran",
                still_missing.join(", ")
            )));
        }
        Ok(ForcedIndexCheck {
            checked: FORCED_INDEXES.len(),
            rebuilt: missing,
        })
    }

    fn missing_forced_indexes(&self) -> Result<Vec<&'static str>, SqliteError> {
        let reader = self.pool.reader()?;
        let mut statement = reader.conn().prepare(FORCED_INDEX_EXISTS)?;
        let mut missing = Vec::new();
        for index in &FORCED_INDEXES {
            let exists: bool = statement.query_row([index.name], |row| row.get(0))?;
            if !exists {
                missing.push(index.name);
            }
        }
        Ok(missing)
    }
}
