//! Bounded prune of expendable polling-dispatch audit rows from an events store.
//!
//! This runs ADR-212 §4's historical classifier, `poll-dispatch-legacy-v1`, as an operator maintenance tool in three
//! phases: a read-only plan that reports counts and a candidate-set digest, a consistent backup through the SQLite
//! backup API, and an apply pinned to the plan's cutoff and digest that deletes in bounded transactions. It is an
//! interim tool. The rest of ADR-212 (two-half reservations, the Expended disposition, protocol negotiation, in-store
//! expended-ID records) is not implemented here; the append-only expended-ID file the apply writes outside the store
//! stands in for §3's permanent records until it is.
//!
//! The tool never runs a migration, never VACUUMs and never changes `auto_vacuum`. Deleted rows leave free pages in
//! the file, and later writes reuse them, so the file stops growing until the free pages are spent; a physical shrink
//! is a separate operation.
//!
//! A row with persisted observations is retained. The generic dispatch producer writes no observations, so any
//! observation on such a row is content this classifier does not know, which ADR-212 §2 says refuses the row.
//!
//! Operating preconditions. The edge reference check reads the edge stores on their own connections, so it cannot share
//! a transaction with the events delete: an edge that names a candidate and commits after a batch's check but before
//! its delete is not seen. ADR-212 §3 closes that window with main-writer reservations, which this tool does not
//! implement, so the apply is run with no process writing edges to the named stores. The edge check is the classifier's
//! last test, so the plan's `edge_referenced` retained count is exactly the rows an edge kept out of an otherwise
//! complete selection (zero is the expected value for polling-dispatch audits). The plan opens every store read-only
//! and issues no write statement; SQLite may still touch a store's existing `-shm` file, so the claim is "no database
//! writes", never "no file-system writes".

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::backup::{Backup, StepResult};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

/// The historical classifier this module implements (ADR-212 §4).
pub const CLASSIFIER: &str = "poll-dispatch-legacy-v1";

/// The fixed horizon of the policy (ADR-212 §2): seven 24-hour days.
pub const HORIZON_SECONDS: i64 = 604_800;

const HORIZON_US: i64 = HORIZON_SECONDS * 1_000_000;
const DAY_US: i64 = 86_400 * 1_000_000;

/// The closed verb set of ADR-212 §1, compared exactly and always together with kind `audit`.
pub const EXPENDABLE_VERBS: [&str; 6] = [
    "comm.inbox",
    "comm.cleanup_expired_quarantine",
    "whoami",
    "comm.probe",
    "comm.cursor_get",
    "comm.heartbeat",
];

/// Rows per delete transaction unless the caller asks for fewer or more.
pub const DEFAULT_BATCH_ROWS: usize = 2_000;

/// The largest delete transaction the apply accepts.
pub const MAX_BATCH_ROWS: usize = 10_000;

/// The decisions a generic dispatch envelope can carry.
const GENERIC_DECISIONS: [&str; 3] = ["allow", "deny", "gate_unavailable"];

/// The `events` columns this code knows, in table order.
const EVENTS_COLUMNS: [&str; 18] = [
    "id",
    "namespace",
    "verb",
    "substrate",
    "actor",
    "kind",
    "outcome",
    "payload",
    "payload_schema_version",
    "profile_state_version",
    "duration_us",
    "target_id",
    "session_id",
    "aggregate_kind",
    "aggregate_id",
    "created_at",
    "op_index",
    "ref_resolution",
];

/// The `event_observations` columns this code knows, in table order.
const OBSERVATION_COLUMNS: [&str; 5] =
    ["event_id", "entity_id", "referent_kind", "role", "position"];

/// How many times a batch that found the store busy is retried before the run stops resumable.
const MAX_BUSY_RETRIES: u32 = 20;

/// Why the prune stopped. Each variant maps to its own process exit code.
#[derive(Debug, thiserror::Error)]
pub enum PruneError {
    /// The store's schema is not one this code knows; nothing was read past the schema check.
    #[error("refused-schema: {0}")]
    Schema(String),
    /// The recomputed candidate set differs from the one the operator authorized.
    #[error("digest-mismatch: expected {expected}, recomputed {actual} ({detail})")]
    DigestMismatch {
        /// The digest passed to the apply.
        expected: String,
        /// The digest of the set selected now, together with the ids already removed by this run.
        actual: String,
        /// Counts behind the recomputed digest.
        detail: String,
    },
    /// No verified backup covers this apply.
    #[error("no-backup: {0}")]
    NoBackup(String),
    /// A control count changed across the apply.
    #[error("control-mismatch: {0}")]
    ControlMismatch(String),
    /// The run stopped before its last batch; rerunning the same command resumes it.
    #[error("interrupted-resumable: {0}")]
    Interrupted(String),
    /// An argument or a state file was refused.
    #[error("invalid: {0}")]
    Invalid(String),
    /// SQLite reported an error.
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A file could not be read or written.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// A state file did not parse.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

impl PruneError {
    /// The process exit code for this outcome: 3 refused-schema, 4 digest-mismatch, 5 no-backup,
    /// 6 control-mismatch, 7 interrupted-resumable, 1 for every other failure.
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Schema(_) => 3,
            Self::DigestMismatch { .. } => 4,
            Self::NoBackup(_) => 5,
            Self::ControlMismatch(_) => 6,
            Self::Interrupted(_) => 7,
            Self::Invalid(_) | Self::Sqlite(_) | Self::Io(_) | Self::Json(_) => 1,
        }
    }
}

/// The stores one run reads.
#[derive(Debug, Clone)]
pub struct PruneTarget {
    /// The events store the prune deletes from.
    pub events_db: PathBuf,
    /// Stores whose `graph_edges` are checked for references to a candidate, opened read-only.
    pub edge_dbs: Vec<PathBuf>,
}

/// Why an audit row of an expendable verb is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetainReason {
    /// Created at or after the cutoff.
    Younger,
    /// The id is not a lowercase hyphenated UUID, or the payload is not a JSON object.
    Malformed,
    /// The row carries an aggregate identity.
    Aggregate,
    /// The payload is not the generic dispatch envelope for this row's verb.
    NonGenericEnvelope,
    /// The payload carries a retention stamp other than the generic producer's exact one.
    RetentionStampMismatch,
    /// The event has persisted observations.
    ObservationsPresent,
    /// A graph edge in one of the edge stores names the event.
    EdgeReferenced,
}

impl RetainReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Younger => "younger",
            Self::Malformed => "malformed",
            Self::Aggregate => "aggregate",
            Self::NonGenericEnvelope => "non_generic_envelope",
            Self::RetentionStampMismatch => "retention_stamp_mismatch",
            Self::ObservationsPresent => "observations_present",
            Self::EdgeReferenced => "edge_referenced",
        }
    }
}

/// One selected row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Candidate {
    /// The event id.
    pub id: Uuid,
    /// Index into [`EXPENDABLE_VERBS`].
    pub verb: u8,
    /// Index into [`Plan::namespaces`].
    pub namespace: u32,
    /// The row's `created_at`, microseconds since the Unix epoch.
    pub created_at: i64,
}

/// Row count for one `(kind, verb)` key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyCount {
    /// The row kind.
    pub kind: String,
    /// The row verb.
    pub verb: String,
    /// Rows created before the cutoff.
    pub rows_before_cutoff: u64,
    /// All rows.
    pub rows: u64,
}

/// Page accounting of the events store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct PageInfo {
    /// `PRAGMA page_size`.
    pub page_size: i64,
    /// `PRAGMA page_count`.
    pub page_count: i64,
    /// `PRAGMA freelist_count`.
    pub freelist_count: i64,
    /// `PRAGMA auto_vacuum` (0 none, 1 full, 2 incremental).
    pub auto_vacuum: i64,
}

/// File sizes and page accounting at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct StoreSize {
    /// Bytes of the store file.
    pub db_bytes: u64,
    /// Bytes of its `-wal` file, 0 when absent.
    pub wal_bytes: u64,
    /// Page accounting.
    pub pages: PageInfo,
}

/// What a run would remove, and why everything else stays.
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    /// Always [`CLASSIFIER`].
    pub classifier: &'static str,
    /// The run's start, microseconds since the Unix epoch.
    pub run_started_us: i64,
    /// Rows created strictly before this instant are age-eligible.
    pub cutoff_us: i64,
    /// The `events` and `event_observations` columns read from the store.
    pub schema: BTreeMap<String, Vec<String>>,
    /// Number of selected rows.
    pub candidates: u64,
    /// Selected rows by verb, then by age at the run's start.
    pub candidates_by_verb_and_age: BTreeMap<String, BTreeMap<String, u64>>,
    /// Audit rows of the six verbs that stay, by reason.
    pub retained_by_reason: BTreeMap<String, u64>,
    /// Age-eligible rows retained for a protection reason (everything retained except `younger`).
    pub protected_backlog: u64,
    /// Every `(kind, verb)` key outside the candidate class, with its row counts.
    pub non_candidate_keys: Vec<KeyCount>,
    /// All rows by kind.
    pub rows_by_kind: BTreeMap<String, u64>,
    /// All rows.
    pub total_rows: u64,
    /// Sizes of the store at the end of the plan.
    pub size: StoreSize,
    /// The candidate-set digest of ADR-212 §4.
    pub digest: String,
    /// Namespaces of the selected rows.
    #[serde(skip)]
    pub namespaces: Vec<String>,
    /// The selected rows, oldest first.
    #[serde(skip)]
    pub selected: Vec<Candidate>,
}

/// Open a store read-only without creating it. A WAL store with a live writer is read through the writer's shared
/// memory, as any SQLite reader does.
fn open_read_only(path: &Path) -> Result<Connection, PruneError> {
    if !path.is_file() {
        return Err(PruneError::Invalid(format!(
            "{} is not a file",
            path.display()
        )));
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "query_only", true)?;
    Ok(conn)
}

/// Open the events store for the apply: read-write, never created.
fn open_read_write(path: &Path, busy_timeout: Duration) -> Result<Connection, PruneError> {
    if !path.is_file() {
        return Err(PruneError::Invalid(format!(
            "{} is not a file",
            path.display()
        )));
    }
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(busy_timeout)?;
    Ok(conn)
}

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>, PruneError> {
    let mut stmt = conn.prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid")?;
    let names = stmt
        .query_map([table], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(names)
}

/// Read the schema identity of the events store and refuse any shape this code does not know.
fn check_schema(conn: &Connection) -> Result<BTreeMap<String, Vec<String>>, PruneError> {
    let events = table_columns(conn, "events")?;
    let observations = table_columns(conn, "event_observations")?;
    let mut read = BTreeMap::new();
    read.insert("events".to_string(), events.clone());
    read.insert("event_observations".to_string(), observations.clone());
    if events != EVENTS_COLUMNS || observations != OBSERVATION_COLUMNS {
        return Err(PruneError::Schema(format!(
            "events columns {events:?}, event_observations columns {observations:?}; this build knows \
             events {EVENTS_COLUMNS:?} and event_observations {OBSERVATION_COLUMNS:?}"
        )));
    }
    Ok(read)
}

/// Every UUID that a graph edge in the given stores names at either endpoint, live or soft-deleted. Endpoints that
/// are not UUIDs cannot name an event and are skipped; parsing makes the match independent of letter case.
fn load_edge_endpoints(edge_dbs: &[PathBuf]) -> Result<HashSet<Uuid>, PruneError> {
    if edge_dbs.is_empty() {
        return Err(PruneError::Invalid(
            "at least one edge store is required for the reference check".into(),
        ));
    }
    let mut endpoints = HashSet::new();
    for path in edge_dbs {
        let conn = open_read_only(path)?;
        let has_edges: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'graph_edges')",
            [],
            |row| row.get(0),
        )?;
        if !has_edges {
            return Err(PruneError::Invalid(format!(
                "{} has no graph_edges table, so its references cannot be checked",
                path.display()
            )));
        }
        let mut stmt = conn.prepare("SELECT source_id, target_id FROM graph_edges")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            for column in 0..2 {
                let value: Option<String> = row.get(column)?;
                if let Some(id) = value.as_deref().and_then(|text| Uuid::parse_str(text).ok()) {
                    endpoints.insert(id);
                }
            }
        }
    }
    Ok(endpoints)
}

/// One audit row of an expendable verb, as stored.
struct AuditRow {
    id: String,
    verb: String,
    created_at: i64,
    payload: String,
    aggregate_kind: Option<String>,
    aggregate_id: Option<String>,
}

/// Apply the ADR-212 §4 predicate to one row whose kind is `audit` and whose verb is one of the six. Returns the
/// parsed id when the row is selected.
fn classify(
    row: &AuditRow,
    cutoff_us: i64,
    has_observations: bool,
    edges: &HashSet<Uuid>,
) -> Result<Uuid, RetainReason> {
    if row.created_at >= cutoff_us {
        return Err(RetainReason::Younger);
    }
    let id = match Uuid::parse_str(&row.id) {
        Ok(id) if id.hyphenated().to_string() == row.id => id,
        _ => return Err(RetainReason::Malformed),
    };
    if row.aggregate_kind.is_some() || row.aggregate_id.is_some() {
        return Err(RetainReason::Aggregate);
    }
    let payload = match serde_json::from_str::<Value>(&row.payload) {
        Ok(Value::Object(map)) => map,
        _ => return Err(RetainReason::Malformed),
    };
    let generic = payload.get("gate_impl").is_some_and(Value::is_string)
        && payload
            .get("decision")
            .and_then(Value::as_str)
            .is_some_and(|decision| GENERIC_DECISIONS.contains(&decision))
        && payload.get("verb").and_then(Value::as_str) == Some(row.verb.as_str());
    if !generic {
        return Err(RetainReason::NonGenericEnvelope);
    }
    if let Some(stamp) = payload.get("retention") {
        let expected = serde_json::json!({
            "class": "expendable",
            "policy": "poll-dispatch-v1",
            "horizon_seconds": HORIZON_SECONDS
        });
        if *stamp != expected {
            return Err(RetainReason::RetentionStampMismatch);
        }
    }
    if has_observations {
        return Err(RetainReason::ObservationsPresent);
    }
    if edges.contains(&id) {
        return Err(RetainReason::EdgeReferenced);
    }
    Ok(id)
}

fn verb_index(verb: &str) -> Option<u8> {
    EXPENDABLE_VERBS
        .iter()
        .position(|known| *known == verb)
        .and_then(|index| u8::try_from(index).ok())
}

fn age_bucket(run_started_us: i64, created_at: i64) -> &'static str {
    let age = run_started_us.saturating_sub(created_at);
    if age < 14 * DAY_US {
        "7-14d"
    } else if age < 30 * DAY_US {
        "14-30d"
    } else if age < 90 * DAY_US {
        "30-90d"
    } else {
        ">90d"
    }
}

/// The candidate-set digest of ADR-212 §4: BLAKE3 over the canonical JSON bytes of the deduplicated, byte-sorted
/// array of lowercase hyphenated id strings, as lowercase hex. The array is streamed into the hasher; for an array of
/// UUID strings the streamed bytes equal `khive_types::canonical_json_bytes` of the same array, which a test asserts.
pub fn candidate_digest<I: IntoIterator<Item = Uuid>>(ids: I) -> String {
    let mut sorted: Vec<Uuid> = ids.into_iter().collect();
    // Uuid orders by its bytes, which is the byte order of the lowercase hyphenated strings.
    sorted.sort_unstable();
    sorted.dedup();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"[");
    for (index, id) in sorted.iter().enumerate() {
        if index > 0 {
            hasher.update(b",");
        }
        hasher.update(b"\"");
        hasher.update(id.hyphenated().to_string().as_bytes());
        hasher.update(b"\"");
    }
    hasher.update(b"]");
    hasher.finalize().to_hex().to_string()
}

fn sidecar_bytes(path: &Path, suffix: &str) -> Result<u64, PruneError> {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    match fs::metadata(PathBuf::from(name)) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

fn store_size(conn: &Connection, path: &Path) -> Result<StoreSize, PruneError> {
    let pragma = |name: &str| -> Result<i64, PruneError> {
        Ok(conn.query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))?)
    };
    Ok(StoreSize {
        db_bytes: fs::metadata(path)?.len(),
        wal_bytes: sidecar_bytes(path, "-wal")?,
        pages: PageInfo {
            page_size: pragma("page_size")?,
            page_count: pragma("page_count")?,
            freelist_count: pragma("freelist_count")?,
            auto_vacuum: pragma("auto_vacuum")?,
        },
    })
}

fn is_candidate_key(kind: &str, verb: &str) -> bool {
    kind == "audit" && verb_index(verb).is_some()
}

/// Row counts of every `(kind, verb)` key outside the candidate class, plus the count of candidate-key rows created
/// before the cutoff.
fn key_counts(conn: &Connection, cutoff_us: i64) -> Result<(Vec<KeyCount>, u64), PruneError> {
    let mut stmt = conn.prepare(
        "SELECT kind, verb, SUM(created_at < ?1), COUNT(*) FROM events GROUP BY kind, verb ORDER BY kind, verb",
    )?;
    let mut rows = stmt.query([cutoff_us])?;
    let mut keys = Vec::new();
    let mut candidate_key_old = 0_u64;
    while let Some(row) = rows.next()? {
        let kind: String = row.get(0)?;
        let verb: String = row.get(1)?;
        let old = u64::try_from(row.get::<_, i64>(2)?).unwrap_or(0);
        let all = u64::try_from(row.get::<_, i64>(3)?).unwrap_or(0);
        if is_candidate_key(&kind, &verb) {
            candidate_key_old += old;
        } else {
            keys.push(KeyCount {
                kind,
                verb,
                rows_before_cutoff: old,
                rows: all,
            });
        }
    }
    Ok((keys, candidate_key_old))
}

/// Plan a run that starts now.
pub fn plan(target: &PruneTarget) -> Result<Plan, PruneError> {
    let now = chrono::Utc::now().timestamp_micros();
    plan_at(target, now, now - HORIZON_US)
}

/// Plan a run with an explicit start and cutoff. The cutoff may be earlier than the policy's (an apply pinned to an
/// earlier plan's cutoff) but never later.
pub fn plan_at(
    target: &PruneTarget,
    run_started_us: i64,
    cutoff_us: i64,
) -> Result<Plan, PruneError> {
    let conn = open_read_only(&target.events_db)?;
    plan_on(&conn, target, run_started_us, cutoff_us)
}

fn plan_on(
    conn: &Connection,
    target: &PruneTarget,
    run_started_us: i64,
    cutoff_us: i64,
) -> Result<Plan, PruneError> {
    if cutoff_us > run_started_us - HORIZON_US {
        return Err(PruneError::Invalid(format!(
            "cutoff {cutoff_us} is later than the run start minus {HORIZON_SECONDS} s ({})",
            run_started_us - HORIZON_US
        )));
    }
    let schema = check_schema(conn)?;
    let edges = load_edge_endpoints(&target.edge_dbs)?;

    let mut namespaces: Vec<String> = Vec::new();
    let mut namespace_index: BTreeMap<String, u32> = BTreeMap::new();
    let mut selected = Vec::new();
    let mut by_verb_and_age: BTreeMap<String, BTreeMap<String, u64>> = BTreeMap::new();
    let mut retained: BTreeMap<String, u64> = BTreeMap::new();

    let mut observed =
        conn.prepare("SELECT EXISTS(SELECT 1 FROM event_observations WHERE event_id = ?1)")?;
    let mut stmt = conn.prepare(
        "SELECT id, namespace, verb, created_at, payload, aggregate_kind, aggregate_id FROM events \
         WHERE kind = 'audit' AND verb IN (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    let mut rows = stmt.query(params![
        EXPENDABLE_VERBS[0],
        EXPENDABLE_VERBS[1],
        EXPENDABLE_VERBS[2],
        EXPENDABLE_VERBS[3],
        EXPENDABLE_VERBS[4],
        EXPENDABLE_VERBS[5],
    ])?;
    while let Some(row) = rows.next()? {
        let namespace: String = row.get(1)?;
        let audit = AuditRow {
            id: row.get(0)?,
            verb: row.get(2)?,
            created_at: row.get(3)?,
            payload: row.get(4)?,
            aggregate_kind: row.get(5)?,
            aggregate_id: row.get(6)?,
        };
        let has_observations = audit.created_at < cutoff_us
            && observed.query_row([&audit.id], |found| found.get::<_, bool>(0))?;
        match classify(&audit, cutoff_us, has_observations, &edges) {
            Ok(id) => {
                let verb = verb_index(&audit.verb).ok_or_else(|| {
                    PruneError::Invalid(format!("selected verb {} is not in the class", audit.verb))
                })?;
                let ns = match namespace_index.get(&namespace) {
                    Some(index) => *index,
                    None => {
                        let index = u32::try_from(namespaces.len())
                            .map_err(|_| PruneError::Invalid("too many namespaces".into()))?;
                        namespaces.push(namespace.clone());
                        namespace_index.insert(namespace, index);
                        index
                    }
                };
                *by_verb_and_age
                    .entry(audit.verb.clone())
                    .or_default()
                    .entry(age_bucket(run_started_us, audit.created_at).to_string())
                    .or_default() += 1;
                selected.push(Candidate {
                    id,
                    verb,
                    namespace: ns,
                    created_at: audit.created_at,
                });
            }
            Err(reason) => *retained.entry(reason.as_str().to_string()).or_default() += 1,
        }
    }
    drop(rows);
    drop(stmt);
    drop(observed);

    selected.sort_unstable_by_key(|candidate| (candidate.created_at, candidate.id));
    let digest = candidate_digest(selected.iter().map(|candidate| candidate.id));
    let protected_backlog = retained
        .iter()
        .filter(|(reason, _)| reason.as_str() != RetainReason::Younger.as_str())
        .map(|(_, count)| *count)
        .sum();
    let (non_candidate_keys, _) = key_counts(conn, cutoff_us)?;
    let rows_by_kind = rows_by_kind(conn)?;
    let total_rows = rows_by_kind.values().sum();
    let size = store_size(conn, &target.events_db)?;

    Ok(Plan {
        classifier: CLASSIFIER,
        run_started_us,
        cutoff_us,
        schema,
        candidates: selected.len() as u64,
        candidates_by_verb_and_age: by_verb_and_age,
        retained_by_reason: retained,
        protected_backlog,
        non_candidate_keys,
        rows_by_kind,
        total_rows,
        size,
        digest,
        namespaces,
        selected,
    })
}

/// Evidence that a backup of the events store exists and was verified, written beside the backup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupReceipt {
    /// Always [`CLASSIFIER`].
    pub classifier: String,
    /// The digest of the plan computed before the backup was taken.
    pub digest: String,
    /// That plan's cutoff.
    pub cutoff_us: i64,
    /// The store that was copied.
    pub source: PathBuf,
    /// The copy.
    pub backup_path: PathBuf,
    /// Bytes of the copy.
    pub bytes: u64,
    /// SHA-256 of the copy, lowercase hex.
    pub sha256: String,
    /// When the copy was taken, microseconds since the Unix epoch.
    pub taken_at_us: i64,
    /// `PRAGMA integrity_check` of the copy.
    pub integrity: Vec<String>,
    /// Rows by kind in the source at copy time; the copy's counts equal these.
    pub rows_by_kind: BTreeMap<String, u64>,
}

fn rows_by_kind(conn: &Connection) -> Result<BTreeMap<String, u64>, PruneError> {
    let mut stmt = conn.prepare("SELECT kind, COUNT(*) FROM events GROUP BY kind")?;
    let mut rows = stmt.query([])?;
    let mut counts = BTreeMap::new();
    while let Some(row) = rows.next()? {
        counts.insert(
            row.get::<_, String>(0)?,
            u64::try_from(row.get::<_, i64>(1)?).unwrap_or(0),
        );
    }
    Ok(counts)
}

fn sha256_file(path: &Path) -> Result<String, PruneError> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// The receipt path written beside a backup.
pub fn receipt_path(backup_path: &Path) -> PathBuf {
    let mut name = backup_path.as_os_str().to_os_string();
    name.push(".receipt.json");
    PathBuf::from(name)
}

/// Copy the events store to `dest` through the SQLite backup API and verify the copy.
///
/// The source rows are counted and the copy is taken inside one read transaction on one connection, so both see the
/// same snapshot while the live writer keeps writing. The whole copy is one backup step: a step-wise copy restarts
/// whenever another connection writes the source, which a live store does continuously. The copy is switched to
/// rollback journaling so it is one self-contained file, then checked with `integrity_check` and recounted. `dest`
/// and its receipt must not exist.
pub fn backup(target: &PruneTarget, plan: &Plan, dest: &Path) -> Result<BackupReceipt, PruneError> {
    let receipt_file = receipt_path(dest);
    for path in [dest, receipt_file.as_path()] {
        if path.exists() {
            return Err(PruneError::Invalid(format!(
                "{} already exists",
                path.display()
            )));
        }
    }
    let source = open_read_only(&target.events_db)?;
    check_schema(&source)?;
    source.execute_batch("BEGIN")?;
    let counted = rows_by_kind(&source)?;
    let taken_at_us = chrono::Utc::now().timestamp_micros();
    {
        let mut copy = Connection::open(dest)?;
        {
            let backup = Backup::new(&source, &mut copy)?;
            let mut attempts = 0_u32;
            loop {
                match backup.step(-1)? {
                    StepResult::Done => break,
                    StepResult::More => {}
                    StepResult::Busy | StepResult::Locked => {
                        attempts += 1;
                        if attempts > MAX_BUSY_RETRIES {
                            return Err(PruneError::Interrupted(
                                "the backup could not lock the source; rerun it".into(),
                            ));
                        }
                        std::thread::sleep(Duration::from_millis(250));
                    }
                    _ => {}
                }
            }
        }
        let mode: String = copy.query_row("PRAGMA journal_mode = DELETE", [], |row| row.get(0))?;
        if mode != "delete" {
            return Err(PruneError::Invalid(format!(
                "the copy kept journal mode {mode}"
            )));
        }
    }
    source.execute_batch("COMMIT")?;
    drop(source);

    let copy = open_read_only(dest)?;
    let mut stmt = copy.prepare(&format!("PRAGMA integrity_check({})", i32::MAX))?;
    let integrity = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    let copied = rows_by_kind(&copy)?;
    drop(copy);
    if integrity != ["ok"] {
        return Err(PruneError::NoBackup(format!(
            "integrity_check of {} reported {integrity:?}",
            dest.display()
        )));
    }
    if copied != counted {
        return Err(PruneError::NoBackup(format!(
            "rows by kind differ: source {counted:?}, copy {copied:?}"
        )));
    }
    let receipt = BackupReceipt {
        classifier: CLASSIFIER.to_string(),
        digest: plan.digest.clone(),
        cutoff_us: plan.cutoff_us,
        source: target.events_db.clone(),
        backup_path: dest.to_path_buf(),
        bytes: fs::metadata(dest)?.len(),
        sha256: sha256_file(dest)?,
        taken_at_us,
        integrity,
        rows_by_kind: counted,
    };
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&receipt_file)?;
    file.write_all(&serde_json::to_vec_pretty(&receipt)?)?;
    file.sync_all()?;
    Ok(receipt)
}

/// How an apply runs.
#[derive(Debug, Clone)]
pub struct ApplyOptions {
    /// The digest of the plan the operator reviewed.
    pub digest: String,
    /// That plan's cutoff; the apply selects against it, not against a fresh one.
    pub cutoff_us: i64,
    /// The receipt of a verified backup taken for this digest and cutoff.
    pub backup_receipt: PathBuf,
    /// Directory holding the checkpoint and the expended-ID file; reusing it resumes the run.
    pub state_dir: PathBuf,
    /// Rows per delete transaction, 1 to [`MAX_BATCH_ROWS`].
    pub batch_rows: usize,
    /// Pause between batches.
    pub pause: Duration,
    /// How long one batch waits for the write lock before it is put back in the queue.
    pub busy_timeout: Duration,
    /// Stop after this many committed batches this invocation (rerun to continue).
    pub max_batches: Option<usize>,
}

/// What an apply did.
#[derive(Debug, Clone, Serialize)]
pub struct ApplyReport {
    /// Always [`CLASSIFIER`].
    pub classifier: &'static str,
    /// The authorized digest.
    pub digest: String,
    /// The pinned cutoff.
    pub cutoff_us: i64,
    /// The backup this apply relied on.
    pub backup_path: PathBuf,
    /// Rows removed by this invocation.
    pub removed: u64,
    /// Rows removed by this run across all invocations, from the expended-ID file.
    pub removed_total: u64,
    /// Selected rows skipped at delete time, by reason.
    pub skipped: BTreeMap<String, u64>,
    /// Batches committed by this invocation.
    pub batches: u64,
    /// Ids an earlier invocation's unfinished batch had removed, found absent at resume.
    pub reconciled: u64,
    /// Every non-candidate key's counts before the first batch.
    pub controls_before: Vec<KeyCount>,
    /// The same counts after the last batch.
    pub controls_after: Vec<KeyCount>,
    /// Candidate-key rows created before the cutoff, before and after.
    pub candidate_key_rows_before_cutoff: [u64; 2],
    /// Sizes before the first batch.
    pub size_before: StoreSize,
    /// Sizes after the last batch.
    pub size_after: StoreSize,
    /// The expended-ID file.
    pub expended_ids_file: PathBuf,
    /// What the expended-ID file stands for.
    pub expended_ids_note: &'static str,
}

/// One line of the expended-ID file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Ledger {
    Intent {
        digest: String,
        batch: u64,
        ids: Vec<LedgerId>,
    },
    Commit {
        digest: String,
        batch: u64,
        removed: Vec<String>,
        skipped: Vec<(String, String)>,
        at_us: i64,
    },
    Reconcile {
        digest: String,
        batch: u64,
        removed: Vec<String>,
        at_us: i64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LedgerId {
    pub(crate) id: String,
    pub(crate) verb: String,
    pub(crate) namespace: String,
    pub(crate) created_at: i64,
}

const EXPENDED_NOTE: &str = "interim stand-in for the permanent expended-ID records of ADR-212 §3; not stored in the \
                             events store and not an archive: the removed payloads are not recoverable from it";

fn append_line(file: &mut fs::File, entry: &Ledger) -> Result<(), PruneError> {
    let mut line = serde_json::to_vec(entry)?;
    line.push(b'\n');
    file.write_all(&line)?;
    file.sync_data()?;
    Ok(())
}

fn read_receipt(path: &Path, digest: &str, cutoff_us: i64) -> Result<BackupReceipt, PruneError> {
    let text = fs::read(path).map_err(|error| {
        PruneError::NoBackup(format!(
            "cannot read backup receipt {}: {error}",
            path.display()
        ))
    })?;
    let receipt: BackupReceipt = serde_json::from_slice(&text)
        .map_err(|error| PruneError::NoBackup(format!("backup receipt does not parse: {error}")))?;
    if receipt.classifier != CLASSIFIER
        || receipt.digest != digest
        || receipt.cutoff_us != cutoff_us
    {
        return Err(PruneError::NoBackup(format!(
            "receipt is for classifier {} digest {} cutoff {}, not this apply's {CLASSIFIER} {digest} {cutoff_us}",
            receipt.classifier, receipt.digest, receipt.cutoff_us
        )));
    }
    if receipt.integrity != ["ok"] {
        return Err(PruneError::NoBackup(format!(
            "receipt records integrity {:?}",
            receipt.integrity
        )));
    }
    let bytes = fs::metadata(&receipt.backup_path)
        .map_err(|error| {
            PruneError::NoBackup(format!(
                "backup {} is not readable: {error}",
                receipt.backup_path.display()
            ))
        })?
        .len();
    if bytes != receipt.bytes || sha256_file(&receipt.backup_path)? != receipt.sha256 {
        return Err(PruneError::NoBackup(format!(
            "backup {} no longer matches its receipt",
            receipt.backup_path.display()
        )));
    }
    Ok(receipt)
}

/// Run the delete phase now.
pub fn apply(target: &PruneTarget, options: &ApplyOptions) -> Result<ApplyReport, PruneError> {
    apply_at(target, options, chrono::Utc::now().timestamp_micros())
}

/// Run the delete phase with an explicit start.
///
/// The selection is recomputed against the pinned cutoff; the ids still selected, together with the ids this run's
/// expended-ID file records as removed, must produce the authorized digest, or nothing is deleted. Each batch writes
/// an intent line, then deletes inside one immediate transaction whose statements recheck the whole predicate, the
/// payload bytes and the edge references per row; a row that changed since selection is skipped and counted. After
/// the commit a commit line follows. An intent line without its commit line (a run killed mid-batch) is settled at
/// the next start by reading which of its ids are still present.
pub fn apply_at(
    target: &PruneTarget,
    options: &ApplyOptions,
    run_started_us: i64,
) -> Result<ApplyReport, PruneError> {
    if options.batch_rows == 0 || options.batch_rows > MAX_BATCH_ROWS {
        return Err(PruneError::Invalid(format!(
            "batch rows {} outside 1..={MAX_BATCH_ROWS}",
            options.batch_rows
        )));
    }
    let receipt = read_receipt(&options.backup_receipt, &options.digest, options.cutoff_us)?;
    if receipt.taken_at_us > run_started_us {
        return Err(PruneError::NoBackup(
            "the backup receipt is dated after this run's start".into(),
        ));
    }

    let conn = open_read_write(&target.events_db, options.busy_timeout)?;
    check_schema(&conn)?;

    fs::create_dir_all(&options.state_dir)?;
    let ledger_path = options.state_dir.join("expended-ids.ndjson");
    let mut intents: BTreeMap<u64, Vec<LedgerId>> = BTreeMap::new();
    let mut removed_ids: HashSet<Uuid> = HashSet::new();
    let mut last_batch = 0_u64;
    if ledger_path.exists() {
        for line in BufReader::new(fs::File::open(&ledger_path)?).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let entry: Ledger = serde_json::from_str(&line)?;
            let (digest, batch) = match &entry {
                Ledger::Intent { digest, batch, .. }
                | Ledger::Commit { digest, batch, .. }
                | Ledger::Reconcile { digest, batch, .. } => (digest.clone(), *batch),
            };
            if digest != options.digest {
                return Err(PruneError::Invalid(format!(
                    "{} belongs to a run for digest {digest}",
                    ledger_path.display()
                )));
            }
            last_batch = last_batch.max(batch);
            match entry {
                Ledger::Intent { batch, ids, .. } => {
                    intents.insert(batch, ids);
                }
                Ledger::Commit { batch, removed, .. }
                | Ledger::Reconcile { batch, removed, .. } => {
                    intents.remove(&batch);
                    for id in removed {
                        removed_ids.insert(Uuid::parse_str(&id).map_err(|_| {
                            PruneError::Invalid(format!("ledger id {id} is not a UUID"))
                        })?);
                    }
                }
            }
        }
    }
    let mut ledger = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&ledger_path)?;

    // Settle batches an earlier invocation started and did not record as committed.
    let mut reconciled = 0_u64;
    {
        let mut present = conn.prepare("SELECT EXISTS(SELECT 1 FROM events WHERE id = ?1)")?;
        for (batch, ids) in std::mem::take(&mut intents) {
            let mut absent = Vec::new();
            for entry in ids {
                if !present.query_row([&entry.id], |row| row.get::<_, bool>(0))? {
                    absent.push(entry.id);
                }
            }
            for id in &absent {
                removed_ids.insert(
                    Uuid::parse_str(id).map_err(|_| {
                        PruneError::Invalid(format!("ledger id {id} is not a UUID"))
                    })?,
                );
            }
            reconciled += absent.len() as u64;
            append_line(
                &mut ledger,
                &Ledger::Reconcile {
                    digest: options.digest.clone(),
                    batch,
                    removed: absent,
                    at_us: chrono::Utc::now().timestamp_micros(),
                },
            )?;
        }
    }

    let planned = plan_on(&conn, target, run_started_us, options.cutoff_us)?;
    let recomputed = candidate_digest(
        planned
            .selected
            .iter()
            .map(|candidate| candidate.id)
            .chain(removed_ids.iter().copied()),
    );
    if recomputed != options.digest {
        return Err(PruneError::DigestMismatch {
            expected: options.digest.clone(),
            actual: recomputed,
            detail: format!(
                "{} selected now, {} removed earlier by this run",
                planned.selected.len(),
                removed_ids.len()
            ),
        });
    }

    let (controls_before, candidate_old_before) = key_counts(&conn, options.cutoff_us)?;
    let size_before = store_size(&conn, &target.events_db)?;

    let mut queue: VecDeque<(u64, Vec<Candidate>, u32)> = planned
        .selected
        .chunks(options.batch_rows)
        .enumerate()
        .map(|(index, chunk)| (last_batch + 1 + index as u64, chunk.to_vec(), 0))
        .collect();
    let mut removed = 0_u64;
    let mut skipped: BTreeMap<String, u64> = BTreeMap::new();
    let mut batches = 0_u64;
    let mut stopped = None;

    while let Some((batch, rows, retries)) = queue.pop_front() {
        if options
            .max_batches
            .is_some_and(|limit| batches as usize >= limit)
        {
            stopped = Some(format!(
                "stopped after {batches} batch(es) as asked; {} batch(es) remain",
                queue.len() + 1
            ));
            break;
        }
        let ids: Vec<LedgerId> = rows
            .iter()
            .map(|candidate| LedgerId {
                id: candidate.id.hyphenated().to_string(),
                verb: EXPENDABLE_VERBS[candidate.verb as usize].to_string(),
                namespace: planned.namespaces[candidate.namespace as usize].clone(),
                created_at: candidate.created_at,
            })
            .collect();
        let edges = load_edge_endpoints(&target.edge_dbs)?;
        // An immediate transaction takes the write lock at BEGIN, waiting at most the busy timeout; a busy store puts
        // the batch at the back of the queue instead of holding the daemon's writer up.
        let began = conn.execute_batch("BEGIN IMMEDIATE");
        if let Err(error) = began {
            if is_busy(&error) {
                if retries + 1 > MAX_BUSY_RETRIES {
                    stopped = Some(format!(
                        "batch {batch} found the store busy {MAX_BUSY_RETRIES} times; rerun to resume"
                    ));
                    queue.push_front((batch, rows, retries));
                    break;
                }
                queue.push_back((batch, rows, retries + 1));
                continue;
            }
            return Err(error.into());
        }
        append_line(
            &mut ledger,
            &Ledger::Intent {
                digest: options.digest.clone(),
                batch,
                ids: ids.clone(),
            },
        )
        .inspect_err(|_| {
            let _ = conn.execute_batch("ROLLBACK");
        })?;
        let outcome = delete_batch(&conn, &ids, options.cutoff_us, &edges);
        let (batch_removed, batch_skipped) = match outcome {
            Ok(result) => result,
            Err(error) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(error);
            }
        };
        if let Err(error) = conn.execute_batch("COMMIT") {
            let _ = conn.execute_batch("ROLLBACK");
            return Err(error.into());
        }
        removed += batch_removed.len() as u64;
        for id in &batch_removed {
            if let Ok(id) = Uuid::parse_str(id) {
                removed_ids.insert(id);
            }
        }
        for (_, reason) in &batch_skipped {
            *skipped.entry(reason.clone()).or_default() += 1;
        }
        append_line(
            &mut ledger,
            &Ledger::Commit {
                digest: options.digest.clone(),
                batch,
                removed: batch_removed,
                skipped: batch_skipped,
                at_us: chrono::Utc::now().timestamp_micros(),
            },
        )?;
        batches += 1;
        let _: (i64, i64, i64) = conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
        if !queue.is_empty() && !options.pause.is_zero() {
            std::thread::sleep(options.pause);
        }
    }

    let (controls_after, candidate_old_after) = key_counts(&conn, options.cutoff_us)?;
    let size_after = store_size(&conn, &target.events_db)?;
    let report = ApplyReport {
        classifier: CLASSIFIER,
        digest: options.digest.clone(),
        cutoff_us: options.cutoff_us,
        backup_path: receipt.backup_path,
        removed,
        removed_total: removed_ids.len() as u64,
        skipped,
        batches,
        reconciled,
        controls_before,
        controls_after,
        candidate_key_rows_before_cutoff: [candidate_old_before, candidate_old_after],
        size_before,
        size_after,
        expended_ids_file: ledger_path,
        expended_ids_note: EXPENDED_NOTE,
    };
    check_controls(
        &report.controls_before,
        &report.controls_after,
        report.candidate_key_rows_before_cutoff,
        report.removed,
    )?;
    if let Some(reason) = stopped {
        return Err(PruneError::Interrupted(reason));
    }
    Ok(report)
}

fn is_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

/// Every non-candidate key keeps its count of rows created before the cutoff, no key's total falls, and the
/// candidate-key rows before the cutoff fall by exactly the rows this invocation removed.
pub(crate) fn check_controls(
    controls_before: &[KeyCount],
    controls_after: &[KeyCount],
    candidate_key_rows_before_cutoff: [u64; 2],
    removed: u64,
) -> Result<(), PruneError> {
    let before: BTreeMap<(&str, &str), &KeyCount> = controls_before
        .iter()
        .map(|key| ((key.kind.as_str(), key.verb.as_str()), key))
        .collect();
    let after: BTreeMap<(&str, &str), &KeyCount> = controls_after
        .iter()
        .map(|key| ((key.kind.as_str(), key.verb.as_str()), key))
        .collect();
    let mut differences = Vec::new();
    for (key, was) in &before {
        match after.get(key) {
            None => differences.push(format!(
                "{}/{}: {} rows, now absent",
                key.0, key.1, was.rows
            )),
            Some(now) => {
                if now.rows_before_cutoff != was.rows_before_cutoff {
                    differences.push(format!(
                        "{}/{}: rows before the cutoff {} -> {}",
                        key.0, key.1, was.rows_before_cutoff, now.rows_before_cutoff
                    ));
                }
                if now.rows < was.rows {
                    differences.push(format!(
                        "{}/{}: rows {} -> {}",
                        key.0, key.1, was.rows, now.rows
                    ));
                }
            }
        }
    }
    for (key, now) in &after {
        if !before.contains_key(key) && now.rows_before_cutoff > 0 {
            differences.push(format!(
                "{}/{}: {} rows before the cutoff appeared",
                key.0, key.1, now.rows_before_cutoff
            ));
        }
    }
    let [old_before, old_after] = candidate_key_rows_before_cutoff;
    if old_before.checked_sub(removed) != Some(old_after) {
        differences.push(format!(
            "candidate-key rows before the cutoff {old_before} -> {old_after}, but {removed} were removed"
        ));
    }
    if differences.is_empty() {
        Ok(())
    } else {
        Err(PruneError::ControlMismatch(differences.join("; ")))
    }
}

/// One batch's result: the removed ids, then each skipped id with the reason it was kept.
pub(crate) type BatchOutcome = (Vec<String>, Vec<(String, String)>);

/// Delete one batch inside the caller's open transaction. Each row is re-read and re-classified against the cutoff
/// and the freshly loaded edge references, then deleted by a statement that repeats the predicate and matches the
/// payload bytes it classified, so a row changed in between is not removed.
pub(crate) fn delete_batch(
    conn: &Connection,
    ids: &[LedgerId],
    cutoff_us: i64,
    edges: &HashSet<Uuid>,
) -> Result<BatchOutcome, PruneError> {
    let mut read = conn.prepare_cached(
        "SELECT kind, verb, created_at, payload, aggregate_kind, aggregate_id, \
         EXISTS(SELECT 1 FROM event_observations WHERE event_id = events.id) FROM events WHERE id = ?1",
    )?;
    let mut delete = conn.prepare_cached(
        "DELETE FROM events WHERE id = ?1 AND kind = 'audit' AND verb = ?2 AND created_at = ?3 \
         AND created_at < ?4 AND payload = ?5 AND aggregate_kind IS NULL AND aggregate_id IS NULL \
         AND NOT EXISTS(SELECT 1 FROM event_observations WHERE event_id = ?1)",
    )?;
    let mut removed = Vec::new();
    let mut skipped = Vec::new();
    for entry in ids {
        let row = read
            .query_row([&entry.id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, bool>(6)?,
                ))
            })
            .optional()?;
        let Some((kind, verb, created_at, payload, aggregate_kind, aggregate_id, observed)) = row
        else {
            skipped.push((entry.id.clone(), "absent".to_string()));
            continue;
        };
        if kind != "audit" || verb != entry.verb || verb_index(&verb).is_none() {
            skipped.push((entry.id.clone(), "changed".to_string()));
            continue;
        }
        let audit = AuditRow {
            id: entry.id.clone(),
            verb,
            created_at,
            payload,
            aggregate_kind,
            aggregate_id,
        };
        if let Err(reason) = classify(&audit, cutoff_us, observed, edges) {
            skipped.push((entry.id.clone(), reason.as_str().to_string()));
            continue;
        }
        let changed = delete.execute(params![
            entry.id,
            audit.verb,
            audit.created_at,
            cutoff_us,
            audit.payload
        ])?;
        if changed == 1 {
            removed.push(entry.id.clone());
        } else {
            skipped.push((entry.id.clone(), "changed".to_string()));
        }
    }
    Ok((removed, skipped))
}
