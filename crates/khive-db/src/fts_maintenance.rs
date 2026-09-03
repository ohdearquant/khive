//! Bounded FTS5 segment maintenance and O(1)-row structure diagnostics.
//!
//! SQLite documents shadow-table row id 10 as the binary structure record.
//! Decoding that row avoids the full `%_idx` scans that would defeat the
//! purpose of an operator diagnostic on a large corpus. Maintenance uses
//! FTS5's incremental `merge` command: a negative page count begins an
//! incremental optimize cycle and later positive counts continue it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, ErrorCode};
use serde::Serialize;

const DEFAULT_MERGE_INTERVAL: Duration = Duration::from_secs(300);
const DEFAULT_MERGE_PAGES: u32 = 500;
const DEFAULT_MINIMUM_SEGMENTS: u64 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FtsLevelStructure {
    pub level: u64,
    pub merge_input_segments: u64,
    pub segment_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FtsIndexStructure {
    pub cookie: u32,
    pub level_count: u64,
    pub segment_count: u64,
    pub level_zero_segments_written: u64,
    pub levels: Vec<FtsLevelStructure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FtsSegmentDiagnostics {
    pub entities: FtsIndexStructure,
    pub notes: FtsIndexStructure,
    pub total_segments: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FtsMaintenanceCounters {
    pub checks: u64,
    pub attempts: u64,
    pub work_steps: u64,
    pub noops: u64,
    pub below_threshold_skips: u64,
    pub busy_skips: u64,
    pub errors: u64,
    pub requested_pages: u64,
}

static FTS_CHECKS: AtomicU64 = AtomicU64::new(0);
static FTS_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
static FTS_WORK_STEPS: AtomicU64 = AtomicU64::new(0);
static FTS_NOOPS: AtomicU64 = AtomicU64::new(0);
static FTS_BELOW_THRESHOLD_SKIPS: AtomicU64 = AtomicU64::new(0);
static FTS_BUSY_SKIPS: AtomicU64 = AtomicU64::new(0);
static FTS_ERRORS: AtomicU64 = AtomicU64::new(0);
static FTS_REQUESTED_PAGES: AtomicU64 = AtomicU64::new(0);

pub fn fts_maintenance_counters() -> FtsMaintenanceCounters {
    FtsMaintenanceCounters {
        checks: FTS_CHECKS.load(Ordering::Relaxed),
        attempts: FTS_ATTEMPTS.load(Ordering::Relaxed),
        work_steps: FTS_WORK_STEPS.load(Ordering::Relaxed),
        noops: FTS_NOOPS.load(Ordering::Relaxed),
        below_threshold_skips: FTS_BELOW_THRESHOLD_SKIPS.load(Ordering::Relaxed),
        busy_skips: FTS_BUSY_SKIPS.load(Ordering::Relaxed),
        errors: FTS_ERRORS.load(Ordering::Relaxed),
        requested_pages: FTS_REQUESTED_PAGES.load(Ordering::Relaxed),
    }
}

#[derive(Debug, Clone)]
pub(crate) struct FtsMaintenanceConfig {
    pub(crate) enabled: bool,
    pub(crate) interval: Duration,
    pub(crate) merge_pages: u32,
    pub(crate) minimum_segments: u64,
}

impl Default for FtsMaintenanceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval: DEFAULT_MERGE_INTERVAL,
            merge_pages: DEFAULT_MERGE_PAGES,
            minimum_segments: DEFAULT_MINIMUM_SEGMENTS,
        }
    }
}

impl FtsMaintenanceConfig {
    pub(crate) fn from_env() -> Self {
        let mut config = Self::default();
        if let Ok(value) = std::env::var("KHIVE_FTS_MERGE_ENABLED") {
            match value.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => config.enabled = true,
                "0" | "false" | "no" | "off" => config.enabled = false,
                _ => tracing::warn!(
                    value,
                    fallback = config.enabled,
                    "invalid KHIVE_FTS_MERGE_ENABLED; using compiled default"
                ),
            }
        }
        if let Ok(value) = std::env::var("KHIVE_FTS_MERGE_INTERVAL_SECS") {
            match value.parse::<u64>() {
                Ok(seconds) if seconds > 0 => config.interval = Duration::from_secs(seconds),
                _ => tracing::warn!(
                    value,
                    fallback_secs = config.interval.as_secs(),
                    "invalid KHIVE_FTS_MERGE_INTERVAL_SECS; using compiled default"
                ),
            }
        }
        if let Ok(value) = std::env::var("KHIVE_FTS_MERGE_PAGES") {
            match value.parse::<u32>() {
                Ok(pages) if pages > 0 => config.merge_pages = pages,
                _ => tracing::warn!(
                    value,
                    fallback_pages = config.merge_pages,
                    "invalid KHIVE_FTS_MERGE_PAGES; using compiled default"
                ),
            }
        }
        if let Ok(value) = std::env::var("KHIVE_FTS_MERGE_MIN_SEGMENTS") {
            match value.parse::<u64>() {
                Ok(segments) if segments >= 2 => config.minimum_segments = segments,
                _ => tracing::warn!(
                    value,
                    fallback_segments = config.minimum_segments,
                    "invalid KHIVE_FTS_MERGE_MIN_SEGMENTS; using compiled default"
                ),
            }
        }
        config
    }
}

#[derive(Debug, Clone, Copy)]
struct FtsTable {
    name: &'static str,
    structure_sql: &'static str,
    merge_sql: &'static str,
}

const FTS_TABLES: [FtsTable; 2] = [
    FtsTable {
        name: "fts_entities",
        structure_sql: "SELECT block FROM fts_entities_data WHERE id = 10",
        merge_sql: "INSERT INTO fts_entities(fts_entities, rank) VALUES('merge', ?1)",
    },
    FtsTable {
        name: "fts_notes",
        structure_sql: "SELECT block FROM fts_notes_data WHERE id = 10",
        merge_sql: "INSERT INTO fts_notes(fts_notes, rank) VALUES('merge', ?1)",
    },
];

#[derive(Debug)]
pub(crate) struct FtsMaintenanceState {
    last_check: Instant,
    next_table: usize,
    in_progress: [bool; FTS_TABLES.len()],
}

impl FtsMaintenanceState {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            last_check: now,
            next_table: 0,
            in_progress: [false; FTS_TABLES.len()],
        }
    }

    /// Whether the next `run_if_due` call would do any work. The checkpoint
    /// loop asks this before moving its connection onto a blocking thread, so
    /// an ordinary tick between maintenance intervals costs one clock read.
    pub(crate) fn is_due(&self, config: &FtsMaintenanceConfig, now: Instant) -> bool {
        config.enabled && now.saturating_duration_since(self.last_check) >= config.interval
    }

    #[cfg(test)]
    fn table_in_progress(&self, table: &str) -> bool {
        FTS_TABLES
            .iter()
            .position(|candidate| candidate.name == table)
            .is_some_and(|index| self.in_progress[index])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FtsMaintenanceOutcome {
    Worked,
    Noop,
    BelowThreshold,
    Busy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FtsMaintenanceStep {
    pub(crate) table: &'static str,
    pub(crate) requested_pages: i64,
    pub(crate) segments_before: u64,
    pub(crate) segments_after: u64,
    pub(crate) outcome: FtsMaintenanceOutcome,
}

fn read_varint(bytes: &[u8], cursor: &mut usize) -> Result<u64, String> {
    let mut value = 0_u64;
    for index in 0..9 {
        let byte = *bytes
            .get(*cursor)
            .ok_or_else(|| "truncated FTS5 structure varint".to_string())?;
        *cursor += 1;
        if index == 8 {
            return value
                .checked_shl(8)
                .map(|prefix| prefix | u64::from(byte))
                .ok_or_else(|| "FTS5 structure varint overflow".to_string());
        }
        value = value
            .checked_shl(7)
            .map(|prefix| prefix | u64::from(byte & 0x7f))
            .ok_or_else(|| "FTS5 structure varint overflow".to_string())?;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    unreachable!("nine-byte SQLite varint returns inside the loop")
}

/// Four-byte marker SQLite writes immediately after the cookie for a V2
/// ("`FTS5_STRUCTURE_V2`") structure record — the layout `contentless_delete=1`
/// tables use. Verified against `fts5StructureWrite`/`fts5StructureDecode` in
/// the vendored `libsqlite3-sys` `sqlite3.c` and against a real record read
/// back from a `contentless_delete=1` table built with this tree's pinned
/// SQLite: cookie(4) + \[V2 marker(4)\] + level_count + segment_count +
/// write_counter, then per level merge_input_segments + level_segments, then
/// per segment segment_id + first_leaf + last_leaf, and for V2 five further
/// varints per segment (origin1, origin2, page-tombstone count,
/// entry-tombstone count, entry count) that this diagnostic does not report.
/// Those five fields are appended after each segment's leaf range, not as a
/// separate counter after the write counter.
const FTS5_STRUCTURE_V2_MARKER: [u8; 4] = [0xFF, 0x00, 0x00, 0x01];

pub(crate) fn parse_structure_record(bytes: &[u8]) -> Result<FtsIndexStructure, String> {
    let cookie_bytes: [u8; 4] = bytes
        .get(..4)
        .ok_or_else(|| "FTS5 structure record is shorter than its 4-byte cookie".to_string())?
        .try_into()
        .expect("slice length checked above");
    let cookie = u32::from_be_bytes(cookie_bytes);
    let mut cursor = 4;
    let is_v2 = bytes.get(cursor..cursor + 4) == Some(&FTS5_STRUCTURE_V2_MARKER[..]);
    if is_v2 {
        cursor += 4;
    }
    let level_count = read_varint(bytes, &mut cursor)?;
    let segment_count = read_varint(bytes, &mut cursor)?;
    let level_zero_segments_written = read_varint(bytes, &mut cursor)?;
    let level_capacity = usize::try_from(level_count)
        .map_err(|_| "FTS5 level count does not fit this platform".to_string())?;
    if level_capacity > bytes.len().saturating_sub(cursor) / 2 {
        return Err(format!(
            "FTS5 structure declares {level_count} levels but only {} bytes remain",
            bytes.len().saturating_sub(cursor)
        ));
    }

    // A V2 segment carries five extra varints (contentless-delete origin and
    // tombstone bookkeeping) after its leaf range; a V1 segment carries none.
    let bytes_per_segment: u64 = if is_v2 { 8 } else { 3 };

    let mut levels = Vec::with_capacity(level_capacity);
    let mut parsed_segments = 0_u64;
    for level in 0..level_count {
        let merge_input_segments = read_varint(bytes, &mut cursor)?;
        let level_segments = read_varint(bytes, &mut cursor)?;
        // A merge consumes the oldest segments of its own level, so the
        // in-progress merge input can never exceed the level's segment count.
        // Accepting a larger value would report a phantom active merge and
        // mask a corrupt or misparsed record.
        if merge_input_segments > level_segments {
            return Err(format!(
                "FTS5 level {level} declares {merge_input_segments} merge input segments but only \
                 {level_segments} segments"
            ));
        }
        let remaining = bytes.len().saturating_sub(cursor);
        if level_segments > remaining as u64 / bytes_per_segment {
            return Err(format!(
                "FTS5 level {level} declares {level_segments} segments but its record is truncated"
            ));
        }
        for _ in 0..level_segments {
            let _segment_id = read_varint(bytes, &mut cursor)?;
            let first_leaf = read_varint(bytes, &mut cursor)?;
            let last_leaf = read_varint(bytes, &mut cursor)?;
            // first_leaf == 0 alone is not corrupt: fts5TrimSegments sets both
            // first_leaf and last_leaf to 0 on a merge input segment once all
            // of its data has been consumed by an in-progress incremental
            // merge (see fts5SegIterInit's pgnoFirst==0 handling in the
            // vendored SQLite source), and the segment stays in the structure
            // record — counted in this level's segment_count and merge input
            // count — until the merge step that finishes it removes it.
            // SQLite's own decoder (fts5StructureDecode) rejects only
            // pgnoLast < pgnoFirst; mirror that exactly.
            if last_leaf < first_leaf {
                return Err(format!(
                    "FTS5 level {level} has invalid leaf range {first_leaf}..={last_leaf}"
                ));
            }
            if is_v2 {
                // origin1, origin2, page-tombstone count, entry-tombstone
                // count, entry count — read (and bounds-checked) so the
                // cursor lands correctly on the next segment/level; this
                // diagnostic reports segment/level counts only.
                for _ in 0..5 {
                    read_varint(bytes, &mut cursor)?;
                }
            }
        }
        parsed_segments = parsed_segments
            .checked_add(level_segments)
            .ok_or_else(|| "FTS5 per-level segment count overflow".to_string())?;
        levels.push(FtsLevelStructure {
            level,
            merge_input_segments,
            segment_count: level_segments,
        });
    }
    if parsed_segments != segment_count {
        return Err(format!(
            "FTS5 structure segment count mismatch: header declares {segment_count}, levels contain {parsed_segments}"
        ));
    }

    Ok(FtsIndexStructure {
        cookie,
        level_count,
        segment_count,
        level_zero_segments_written,
        levels,
    })
}

fn inspect_table(conn: &Connection, table: FtsTable) -> Result<FtsIndexStructure, String> {
    let structure = conn
        .query_row(table.structure_sql, [], |row| row.get::<_, Vec<u8>>(0))
        .map_err(|error| format!("{} structure read failed: {error}", table.name))?;
    parse_structure_record(&structure)
        .map_err(|error| format!("{} structure record is invalid: {error}", table.name))
}

fn has_active_merge(structure: &FtsIndexStructure) -> bool {
    structure
        .levels
        .iter()
        .any(|level| level.merge_input_segments > 0)
}

/// Look up an `FTS_TABLES` entry by name instead of by array position, so a
/// future reorder of `FTS_TABLES` cannot silently swap which structure record
/// a diagnostic caller reads. The scheduler (`run_if_due`'s round-robin)
/// stays position-based deliberately — it treats every entry uniformly and
/// does not attach meaning to a specific index.
fn fts_table(name: &str) -> FtsTable {
    FTS_TABLES
        .iter()
        .find(|table| table.name == name)
        .copied()
        .unwrap_or_else(|| unreachable!("FTS_TABLES must define a {name} entry"))
}

pub(crate) fn inspect_fts_segments(conn: &Connection) -> Result<FtsSegmentDiagnostics, String> {
    let entities = inspect_table(conn, fts_table("fts_entities"))?;
    let notes = inspect_table(conn, fts_table("fts_notes"))?;
    let total_segments = entities.segment_count.saturating_add(notes.segment_count);
    Ok(FtsSegmentDiagnostics {
        entities,
        notes,
        total_segments,
    })
}

fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

fn run_merge_statement(
    conn: &Connection,
    table: FtsTable,
    requested_pages: i64,
) -> Result<FtsMaintenanceOutcome, String> {
    let prior_busy_ms = conn
        .query_row("PRAGMA busy_timeout", [], |row| row.get::<_, i64>(0))
        .map_err(|error| format!("read busy_timeout before FTS maintenance: {error}"))?;
    conn.busy_timeout(Duration::ZERO)
        .map_err(|error| format!("disable busy wait for FTS maintenance: {error}"))?;
    FTS_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
    FTS_REQUESTED_PAGES.fetch_add(requested_pages.unsigned_abs(), Ordering::Relaxed);
    let changes_before = conn.total_changes();
    let execution = conn.execute(table.merge_sql, params![requested_pages]);
    let restore = conn.busy_timeout(Duration::from_millis(prior_busy_ms.max(0) as u64));
    if let Err(error) = restore {
        FTS_ERRORS.fetch_add(1, Ordering::Relaxed);
        return Err(format!(
            "restore busy_timeout after {} maintenance: {error}",
            table.name
        ));
    }
    match execution {
        Ok(_) => {
            let changes = conn.total_changes().saturating_sub(changes_before);
            if changes >= 2 {
                FTS_WORK_STEPS.fetch_add(1, Ordering::Relaxed);
                Ok(FtsMaintenanceOutcome::Worked)
            } else {
                FTS_NOOPS.fetch_add(1, Ordering::Relaxed);
                Ok(FtsMaintenanceOutcome::Noop)
            }
        }
        Err(error) if is_busy(&error) => {
            FTS_BUSY_SKIPS.fetch_add(1, Ordering::Relaxed);
            Ok(FtsMaintenanceOutcome::Busy)
        }
        Err(error) => {
            FTS_ERRORS.fetch_add(1, Ordering::Relaxed);
            Err(format!("{} bounded merge failed: {error}", table.name))
        }
    }
}

pub(crate) fn run_if_due(
    conn: &Connection,
    config: &FtsMaintenanceConfig,
    state: &mut FtsMaintenanceState,
    now: Instant,
) -> Result<Option<FtsMaintenanceStep>, String> {
    if !state.is_due(config, now) {
        return Ok(None);
    }
    state.last_check = now;
    let table_index = state.next_table;
    state.next_table = (state.next_table + 1) % FTS_TABLES.len();
    let table = FTS_TABLES[table_index];
    FTS_CHECKS.fetch_add(1, Ordering::Relaxed);

    let before = match inspect_table(conn, table) {
        Ok(structure) => structure,
        Err(error) => {
            FTS_ERRORS.fetch_add(1, Ordering::Relaxed);
            return Err(error);
        }
    };
    // The structure record is authoritative across daemon restarts. Local
    // state avoids treating an implementation-specific structure transition
    // as completion, while nMerge resumes a persisted incremental optimize.
    let merge_in_progress = state.in_progress[table_index] || has_active_merge(&before);
    if !merge_in_progress && before.segment_count < config.minimum_segments {
        FTS_BELOW_THRESHOLD_SKIPS.fetch_add(1, Ordering::Relaxed);
        return Ok(Some(FtsMaintenanceStep {
            table: table.name,
            requested_pages: 0,
            segments_before: before.segment_count,
            segments_after: before.segment_count,
            outcome: FtsMaintenanceOutcome::BelowThreshold,
        }));
    }

    let page_budget = i64::from(config.merge_pages);
    let requested_pages = if merge_in_progress {
        page_budget
    } else {
        -page_budget
    };
    let outcome = run_merge_statement(conn, table, requested_pages)?;
    if outcome == FtsMaintenanceOutcome::Busy {
        return Ok(Some(FtsMaintenanceStep {
            table: table.name,
            requested_pages,
            segments_before: before.segment_count,
            segments_after: before.segment_count,
            outcome,
        }));
    }

    let after = match inspect_table(conn, table) {
        Ok(structure) => structure,
        Err(error) => {
            state.in_progress[table_index] = outcome == FtsMaintenanceOutcome::Worked;
            FTS_ERRORS.fetch_add(1, Ordering::Relaxed);
            return Err(error);
        }
    };
    state.in_progress[table_index] = outcome == FtsMaintenanceOutcome::Worked
        && (after.segment_count > 1 || has_active_merge(&after));
    Ok(Some(FtsMaintenanceStep {
        table: table.name,
        requested_pages,
        segments_before: before.segment_count,
        segments_after: after.segment_count,
        outcome,
    }))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use rusqlite::{params, Connection};
    use tempfile::TempDir;

    use super::*;

    fn create_fts_tables(conn: &Connection) {
        conn.execute_batch(
            "CREATE VIRTUAL TABLE fts_entities USING fts5(namespace UNINDEXED, subject_id UNINDEXED, title, body, tokenize='trigram');
             CREATE VIRTUAL TABLE fts_notes USING fts5(namespace UNINDEXED, subject_id UNINDEXED, title, body, tokenize='trigram');
             INSERT INTO fts_entities(fts_entities, rank) VALUES('automerge', 0);
             INSERT INTO fts_notes(fts_notes, rank) VALUES('automerge', 0);",
        )
        .expect("create FTS fixtures");
    }

    fn seed_segments(conn: &Connection, table: &str, count: usize) {
        let sql = format!(
            "INSERT INTO {table}(namespace, subject_id, title, body) VALUES(?1, ?2, ?3, ?4)"
        );
        for index in 0..count {
            let body = format!(
                "segment fixture {index} keeps enough repeated production recall text to span pages {}",
                "memory query corpus ".repeat(40)
            );
            conn.execute(
                &sql,
                params![
                    "local",
                    format!("id-{index}"),
                    format!("title {index}"),
                    body
                ],
            )
            .expect("one autocommit FTS write");
        }
    }

    fn file_fixture() -> (TempDir, Connection, Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("fts-maintenance.db");
        let maintenance = Connection::open(&path).expect("maintenance connection");
        maintenance
            .pragma_update(None, "journal_mode", "WAL")
            .expect("enable WAL");
        create_fts_tables(&maintenance);
        let writer = Connection::open(&path).expect("writer connection");
        (dir, maintenance, writer)
    }

    fn push_varint(mut value: u64, out: &mut Vec<u8>) {
        let mut bytes = [0_u8; 10];
        let mut cursor = bytes.len();
        cursor -= 1;
        bytes[cursor] = (value & 0x7f) as u8;
        value >>= 7;
        while value != 0 {
            cursor -= 1;
            bytes[cursor] = ((value & 0x7f) as u8) | 0x80;
            value >>= 7;
        }
        out.extend_from_slice(&bytes[cursor..]);
    }

    #[test]
    fn parses_documented_structure_record_with_multibyte_segment_count() {
        let mut bytes = vec![0, 0, 0, 7];
        push_varint(2, &mut bytes);
        push_varint(130, &mut bytes);
        push_varint(321, &mut bytes);
        push_varint(3, &mut bytes);
        push_varint(70, &mut bytes);
        for segment in 0..70 {
            push_varint(segment + 1, &mut bytes);
            push_varint(1, &mut bytes);
            push_varint(2, &mut bytes);
        }
        push_varint(0, &mut bytes);
        push_varint(60, &mut bytes);
        for segment in 0..60 {
            push_varint(segment + 71, &mut bytes);
            push_varint(1, &mut bytes);
            push_varint(1, &mut bytes);
        }

        let parsed = parse_structure_record(&bytes).expect("valid documented structure record");

        assert_eq!(parsed.cookie, 7);
        assert_eq!(parsed.level_count, 2);
        assert_eq!(parsed.segment_count, 130);
        assert_eq!(parsed.level_zero_segments_written, 321);
        assert_eq!(parsed.levels[0].merge_input_segments, 3);
        assert_eq!(parsed.levels[0].segment_count, 70);
        assert_eq!(parsed.levels[1].segment_count, 60);
    }

    #[test]
    fn parses_v2_contentless_delete_structure_record() {
        // Captured verbatim from `%_data.id = 10` of a real
        // `content='', contentless_delete=1` FTS5 table, built under this
        // tree's pinned SQLite, after two single-page inserts:
        // `sqlite3 v2.db "CREATE VIRTUAL TABLE t USING
        // fts5(body, content='', contentless_delete=1, tokenize='trigram');
        // INSERT ...; INSERT ...; SELECT hex(block) FROM t_data WHERE id=10"`
        // -> 00000000FF000001010202000201010101010000010201010202000001
        //
        // cookie(4)=0, V2 marker(4)=FF 00 00 01, level_count=1,
        // segment_count=2, write_counter=2; one level with
        // merge_input_segments=0, level_segments=2; two segments, each
        // segment_id/first_leaf/last_leaf followed by the five V2-only
        // varints (origin1, origin2, page-tombstone count, entry-tombstone
        // count, entry count) `fts5StructureWrite` appends for
        // `contentless_delete=1` tables. There is no extra counter inserted
        // after the write counter — verified against both the vendored
        // `fts5StructureDecode`/`fts5StructureWrite` source and this real
        // record's byte length (29 bytes: 8-byte header + 3-byte level
        // header + 2 segments * 8 bytes).
        let bytes = [
            0x00, 0x00, 0x00, 0x00, 0xff, 0x00, 0x00, 0x01, 0x01, 0x02, 0x02, 0x00, 0x02, 0x01,
            0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x01, 0x02, 0x01, 0x01, 0x02, 0x02, 0x00, 0x00,
            0x01,
        ];

        let parsed = parse_structure_record(&bytes).expect("valid V2 structure record");

        assert_eq!(parsed.cookie, 0);
        assert_eq!(parsed.level_count, 1);
        assert_eq!(parsed.segment_count, 2);
        assert_eq!(parsed.level_zero_segments_written, 2);
        assert_eq!(parsed.levels.len(), 1);
        assert_eq!(parsed.levels[0].merge_input_segments, 0);
        assert_eq!(parsed.levels[0].segment_count, 2);
    }

    #[test]
    fn v1_structure_record_is_unaffected_by_v2_marker_detection() {
        // A V1 record's byte 4 is the top byte of the level-count varint,
        // never 0xFF for a small level count, so the V2 marker check must
        // not misfire on ordinary records — regression guard for the
        // marker-detection addition above.
        let mut bytes = vec![0, 0, 0, 9];
        push_varint(1, &mut bytes); // level_count
        push_varint(1, &mut bytes); // segment_count
        push_varint(5, &mut bytes); // write_counter
        push_varint(0, &mut bytes); // level 0 merge_input_segments
        push_varint(1, &mut bytes); // level 0 level_segments
        push_varint(1, &mut bytes); // segment_id
        push_varint(1, &mut bytes); // first_leaf
        push_varint(1, &mut bytes); // last_leaf

        let parsed = parse_structure_record(&bytes).expect("valid V1 structure record");
        assert_eq!(parsed.cookie, 9);
        assert_eq!(parsed.segment_count, 1);
    }

    #[test]
    fn rejects_truncated_or_self_inconsistent_structure_records() {
        assert!(parse_structure_record(&[0, 0, 0]).is_err());

        let inconsistent = [0, 0, 0, 0, 1, 2, 0, 0, 1, 1, 1, 1];
        let error = parse_structure_record(&inconsistent)
            .expect_err("declared total must equal per-level total");
        assert!(error.contains("segment count"), "unexpected error: {error}");
    }

    #[test]
    fn rejects_merge_input_exceeding_the_level_segment_count() {
        // One level, two segments, but a merge claiming three inputs: the
        // record is self-inconsistent and must surface as a parse error
        // rather than as an active merge over segments that do not exist.
        let mut bytes = vec![0, 0, 0, 0];
        push_varint(1, &mut bytes); // level_count
        push_varint(2, &mut bytes); // segment_count
        push_varint(2, &mut bytes); // write_counter
        push_varint(3, &mut bytes); // level 0 merge_input_segments (> level_segments)
        push_varint(2, &mut bytes); // level 0 level_segments
        for segment in 1..=2 {
            push_varint(segment, &mut bytes);
            push_varint(1, &mut bytes);
            push_varint(1, &mut bytes);
        }

        let error = parse_structure_record(&bytes)
            .expect_err("merge input segments must not exceed the level's segment count");
        assert!(error.contains("merge input"), "unexpected error: {error}");

        // The same record with a consistent merge input parses and reports
        // the merge, so the guard rejects only the impossible value.
        bytes[7] = 2;
        let parsed = parse_structure_record(&bytes).expect("consistent record parses");
        assert_eq!(parsed.levels[0].merge_input_segments, 2);
        assert!(has_active_merge(&parsed));
    }

    #[test]
    fn accepts_a_trimmed_merge_input_segment() {
        // One level, two segments, both inputs to an active two-segment
        // merge. The first has been fully "trimmed" by fts5TrimSegments
        // (first_leaf == last_leaf == 0, exactly as SQLite writes it once an
        // input segment's data has all been transferred to the merge output
        // but the merge step has not yet finished); the second still has
        // live leaf pages. Expected: this decodes and reports an active
        // merge, matching SQLite's own fts5StructureDecode, which rejects
        // only pgnoLast < pgnoFirst and never checks pgnoFirst == 0.
        let mut bytes = vec![0, 0, 0, 0];
        push_varint(1, &mut bytes); // level_count
        push_varint(2, &mut bytes); // segment_count
        push_varint(5, &mut bytes); // write_counter
        push_varint(2, &mut bytes); // level 0 merge_input_segments
        push_varint(2, &mut bytes); // level 0 level_segments
        push_varint(1, &mut bytes); // segment 1 id
        push_varint(0, &mut bytes); // segment 1 first_leaf (trimmed)
        push_varint(0, &mut bytes); // segment 1 last_leaf (trimmed)
        push_varint(2, &mut bytes); // segment 2 id
        push_varint(5, &mut bytes); // segment 2 first_leaf
        push_varint(9, &mut bytes); // segment 2 last_leaf

        let parsed =
            parse_structure_record(&bytes).expect("a trimmed input segment must not be corrupt");

        assert_eq!(parsed.levels[0].segment_count, 2);
        assert_eq!(parsed.levels[0].merge_input_segments, 2);
        assert!(has_active_merge(&parsed));
    }

    #[test]
    fn rejects_last_leaf_before_first_leaf() {
        // Control for the trimmed-segment acceptance above: a segment whose
        // last_leaf is less than a nonzero first_leaf is still corrupt, the
        // one case SQLite's own decoder rejects.
        let mut bytes = vec![0, 0, 0, 0];
        push_varint(1, &mut bytes); // level_count
        push_varint(1, &mut bytes); // segment_count
        push_varint(2, &mut bytes); // write_counter
        push_varint(0, &mut bytes); // level 0 merge_input_segments
        push_varint(1, &mut bytes); // level 0 level_segments
        push_varint(1, &mut bytes); // segment id
        push_varint(5, &mut bytes); // first_leaf
        push_varint(3, &mut bytes); // last_leaf < first_leaf

        let error = parse_structure_record(&bytes)
            .expect_err("last_leaf below first_leaf must still be rejected");
        assert!(
            error.contains("invalid leaf range"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn segment_diagnostics_use_one_structure_row_and_never_scan_the_idx_table() {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        create_fts_tables(&conn);
        seed_segments(&conn, "fts_notes", 8);

        let mut statement = conn
            .prepare(&format!(
                "EXPLAIN QUERY PLAN {}",
                FTS_TABLES[1].structure_sql
            ))
            .expect("explain structure lookup");
        let details: Vec<String> = statement
            .query_map([], |row| row.get(3))
            .expect("query plan rows")
            .collect::<Result<_, _>>()
            .expect("query plan details");

        assert!(
            details
                .iter()
                .all(|detail| !detail.contains("fts_notes_idx")),
            "segment diagnostics must not scan the large idx shadow table: {details:?}"
        );
        assert!(
            !FTS_TABLES[1]
                .structure_sql
                .to_ascii_uppercase()
                .contains("COUNT"),
            "the segment count must come from the structure header, not COUNT(*)"
        );
        assert_eq!(
            inspect_fts_segments(&conn)
                .expect("decode structure record")
                .notes
                .segment_count,
            8
        );
    }

    #[test]
    fn bounded_steps_converge_to_one_segment_without_changing_match_results() {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        create_fts_tables(&conn);
        seed_segments(&conn, "fts_notes", 80);
        let before = inspect_fts_segments(&conn).expect("inspect fragmented fixture");
        assert!(before.notes.segment_count > 1, "fixture must be fragmented");
        let expected_rows: Vec<String> = conn
            .prepare("SELECT subject_id FROM fts_notes WHERE fts_notes MATCH 'production recall' ORDER BY subject_id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        let config = FtsMaintenanceConfig {
            enabled: true,
            interval: Duration::ZERO,
            merge_pages: 8,
            minimum_segments: 2,
        };
        let mut state = FtsMaintenanceState::new(Instant::now());
        for _ in 0..512 {
            let now = Instant::now();
            run_if_due(&conn, &config, &mut state, now).expect("bounded maintenance step");
            if inspect_fts_segments(&conn).unwrap().notes.segment_count == 1 {
                break;
            }
        }

        let after = inspect_fts_segments(&conn).expect("inspect optimized fixture");
        assert_eq!(after.notes.segment_count, 1);
        assert!(
            !has_active_merge(&after.notes),
            "convergence must leave no active merge behind"
        );
        assert!(
            !state.table_in_progress("fts_notes"),
            "convergence must clear the in-progress bookkeeping"
        );
        let actual_rows: Vec<String> = conn
            .prepare("SELECT subject_id FROM fts_notes WHERE fts_notes MATCH 'production recall' ORDER BY subject_id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(actual_rows, expected_rows);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM fts_notes", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            80
        );
    }

    #[test]
    fn maintenance_round_robins_and_continues_with_positive_page_budget() {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        create_fts_tables(&conn);
        seed_segments(&conn, "fts_entities", 80);
        seed_segments(&conn, "fts_notes", 80);
        let config = FtsMaintenanceConfig {
            enabled: true,
            interval: Duration::ZERO,
            merge_pages: 1,
            minimum_segments: 2,
        };
        let mut state = FtsMaintenanceState::new(Instant::now());

        let first = run_if_due(&conn, &config, &mut state, Instant::now())
            .unwrap()
            .expect("first due step");
        let second = run_if_due(&conn, &config, &mut state, Instant::now())
            .unwrap()
            .expect("second due step");
        let third = run_if_due(&conn, &config, &mut state, Instant::now())
            .unwrap()
            .expect("third due step");

        assert_eq!(first.table, "fts_entities");
        assert_eq!(first.requested_pages, -1);
        assert_eq!(second.table, "fts_notes");
        assert_eq!(second.requested_pages, -1);
        assert_eq!(third.table, "fts_entities");
        assert_eq!(third.requested_pages, 1);
    }

    #[test]
    fn persisted_incremental_merge_resumes_after_maintenance_state_restart() {
        let conn = Connection::open_in_memory().expect("in-memory sqlite");
        create_fts_tables(&conn);
        seed_segments(&conn, "fts_entities", 80);
        let config = FtsMaintenanceConfig {
            enabled: true,
            interval: Duration::ZERO,
            merge_pages: 1,
            minimum_segments: 2,
        };
        let mut first_process = FtsMaintenanceState::new(Instant::now());

        let starter = run_if_due(&conn, &config, &mut first_process, Instant::now())
            .expect("starter succeeds")
            .expect("starter is due");
        assert_eq!(starter.requested_pages, -1);
        assert!(
            has_active_merge(&inspect_table(&conn, FTS_TABLES[0]).expect("structure")),
            "one-page starter must persist an incremental merge in FTS5's structure record"
        );

        let mut restarted_process = FtsMaintenanceState::new(Instant::now());
        let resumed = run_if_due(&conn, &config, &mut restarted_process, Instant::now())
            .expect("resume succeeds")
            .expect("resume is due");
        assert_eq!(resumed.table, "fts_entities");
        assert_eq!(
            resumed.requested_pages, 1,
            "persisted nMerge state must continue instead of restarting optimize"
        );
    }

    #[test]
    fn held_writer_is_counted_as_busy_without_losing_the_optimize_cycle() {
        let (_dir, maintenance, writer) = file_fixture();
        seed_segments(&maintenance, "fts_entities", 20);
        writer
            .execute_batch("BEGIN IMMEDIATE")
            .expect("hold writer");
        let counters_before = fts_maintenance_counters();
        let config = FtsMaintenanceConfig {
            enabled: true,
            interval: Duration::ZERO,
            merge_pages: 8,
            minimum_segments: 2,
        };
        let mut state = FtsMaintenanceState::new(Instant::now());

        let step = run_if_due(&maintenance, &config, &mut state, Instant::now())
            .expect("busy is an outcome, not a task failure")
            .expect("due step");

        assert_eq!(step.outcome, FtsMaintenanceOutcome::Busy);
        assert_eq!(
            fts_maintenance_counters().busy_skips,
            counters_before.busy_skips + 1
        );
        assert!(
            !state.table_in_progress("fts_entities"),
            "a busy initial step must retry the negative starter next cadence"
        );
        writer.execute_batch("ROLLBACK").expect("release writer");
    }
}
