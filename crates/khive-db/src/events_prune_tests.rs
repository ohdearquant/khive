//! Tests for `events_prune`. Every store here is a scratch file built by the test.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, Connection};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::events_prune::{
    self, ApplyOptions, KeyCount, PruneError, PruneTarget, EXPENDABLE_VERBS, HORIZON_SECONDS,
};

const EVENTS_DDL: &str = include_str!("../sql/events-ddl.sql");
const RUN_STARTED_US: i64 = 1_800_000_000_000_000;
const DAY_US: i64 = 86_400_000_000;
const CUTOFF_US: i64 = RUN_STARTED_US - HORIZON_SECONDS * 1_000_000;
const OLD_US: i64 = CUTOFF_US - DAY_US;

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    events: PathBuf,
    main: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("scratch directory");
        let root = dir.path().to_path_buf();
        let events = root.join("store.db.events.db");
        let main = root.join("store.db");
        let conn = Connection::open(&events).expect("events store");
        let mode: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
            .expect("wal");
        assert_eq!(mode, "wal");
        conn.execute_batch(EVENTS_DDL).expect("events schema");
        let main_conn = Connection::open(&main).expect("main store");
        main_conn
            .execute_batch(
                "CREATE TABLE graph_edges (namespace TEXT NOT NULL, id TEXT PRIMARY KEY, source_id TEXT NOT NULL, \
                 target_id TEXT NOT NULL, relation TEXT NOT NULL, weight REAL NOT NULL DEFAULT 1.0, \
                 created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, deleted_at INTEGER, metadata TEXT, \
                 target_backend TEXT)",
            )
            .expect("edges table");
        Self {
            _dir: dir,
            root,
            events,
            main,
        }
    }

    fn target(&self) -> PruneTarget {
        PruneTarget {
            events_db: self.events.clone(),
            edge_dbs: vec![self.main.clone()],
        }
    }

    fn conn(&self) -> Connection {
        Connection::open(&self.events).expect("open events store")
    }

    /// Fold the WAL into the main file, so the file's bytes cover every committed row.
    fn checkpoint(&self) {
        let busy: i64 = self
            .conn()
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
            .expect("checkpoint");
        assert_eq!(busy, 0, "the checkpoint was blocked");
    }

    fn insert(
        &self,
        id: &str,
        kind: &str,
        verb: &str,
        created_at: i64,
        payload: &str,
        aggregate: bool,
    ) {
        self.conn()
            .execute(
                "INSERT INTO events (id, namespace, verb, substrate, actor, kind, outcome, payload, created_at, \
                 aggregate_kind, aggregate_id) VALUES (?1, 'local', ?2, 'event', 'lambda:test', ?3, 'success', ?4, ?5, ?6, ?7)",
                params![
                    id,
                    verb,
                    kind,
                    payload,
                    created_at,
                    aggregate.then_some("session"),
                    aggregate.then_some("s-1")
                ],
            )
            .expect("insert event");
    }

    /// A generic dispatch audit row; returns its id.
    fn generic(&self, verb: &str, created_at: i64) -> String {
        let id = Uuid::new_v4().hyphenated().to_string();
        self.insert(
            &id,
            "audit",
            verb,
            created_at,
            &envelope(verb, "allow").to_string(),
            false,
        );
        id
    }

    fn edge(&self, source: &str, target: &str, deleted: bool) {
        Connection::open(&self.main)
            .expect("main store")
            .execute(
                "INSERT INTO graph_edges (namespace, id, source_id, target_id, relation, created_at, updated_at, deleted_at) \
                 VALUES ('local', ?1, ?2, ?3, 'annotates', 1, 1, ?4)",
                params![Uuid::new_v4().to_string(), source, target, deleted.then_some(2_i64)],
            )
            .expect("insert edge");
    }

    fn ids(&self) -> BTreeSet<String> {
        let conn = self.conn();
        let mut stmt = conn.prepare("SELECT id FROM events").expect("prepare");
        let ids = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        ids
    }

    /// Every row except the given ids, as text, ordered by id.
    fn rows_except(&self, skip: &BTreeSet<String>) -> Vec<String> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT id, namespace, verb, kind, outcome, payload, created_at, aggregate_kind FROM events ORDER BY id")
            .expect("prepare");
        let rows: Vec<(String, String)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    format!(
                        "{}|{}|{}|{}|{}|{}|{:?}",
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, Option<String>>(7)?
                    ),
                ))
            })
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        rows.into_iter()
            .filter(|(id, _)| !skip.contains(id))
            .map(|(id, row)| format!("{id}|{row}"))
            .collect()
    }

    fn backup(&self, plan: &events_prune::Plan, name: &str) -> PathBuf {
        let dest = self.root.join(name);
        events_prune::backup(&self.target(), plan, &dest).expect("backup");
        events_prune::receipt_path(&dest)
    }

    fn options(&self, digest: &str, receipt: PathBuf, state: &str) -> ApplyOptions {
        ApplyOptions {
            digest: digest.to_string(),
            cutoff_us: CUTOFF_US,
            backup_receipt: receipt,
            state_dir: self.root.join(state),
            batch_rows: 2,
            pause: Duration::ZERO,
            busy_timeout: Duration::from_millis(200),
            max_batches: None,
        }
    }
}

fn envelope(verb: &str, decision: &str) -> Value {
    json!({
        "timestamp": "2026-01-01T00:00:00Z",
        "actor": {"kind": "lambda", "id": "test"},
        "namespace": "local",
        "verb": verb,
        "decision": decision,
        "obligations": [],
        "gate_impl": "allow_all"
    })
}

fn plan(fixture: &Fixture) -> events_prune::Plan {
    events_prune::plan_at(&fixture.target(), RUN_STARTED_US, CUTOFF_US).expect("plan")
}

fn schema_sql(path: &Path) -> Vec<String> {
    let conn = Connection::open(path).expect("open");
    let mut stmt = conn
        .prepare("SELECT type || ' ' || name || ' ' || IFNULL(sql, '') FROM sqlite_master ORDER BY type, name")
        .expect("prepare");
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("rows");
    rows
}

#[test]
fn each_of_the_six_verbs_is_selected() {
    let fixture = Fixture::new();
    let ids: BTreeSet<String> = EXPENDABLE_VERBS
        .iter()
        .map(|verb| fixture.generic(verb, OLD_US))
        .collect();
    let plan = plan(&fixture);
    assert_eq!(plan.candidates, 6);
    for verb in EXPENDABLE_VERBS {
        assert_eq!(plan.candidates_by_verb_and_age[verb]["7-14d"], 1, "{verb}");
    }
    let selected: BTreeSet<String> = plan
        .selected
        .iter()
        .map(|c| c.id.hyphenated().to_string())
        .collect();
    assert_eq!(selected, ids);
    assert!(
        plan.retained_by_reason.is_empty(),
        "{:?}",
        plan.retained_by_reason
    );
}

#[test]
fn every_excluded_shape_is_retained_with_its_reason() {
    let fixture = Fixture::new();
    let keep = fixture.generic("comm.inbox", OLD_US);
    // Outside the class altogether: a seventh verb, and an allowlisted verb under another kind.
    fixture.insert(
        &Uuid::new_v4().to_string(),
        "audit",
        "knowledge.get",
        OLD_US,
        &envelope("knowledge.get", "allow").to_string(),
        false,
    );
    fixture.insert(
        &Uuid::new_v4().to_string(),
        "phase_started",
        "comm.inbox",
        OLD_US,
        &envelope("comm.inbox", "allow").to_string(),
        false,
    );
    fixture.insert(
        &Uuid::new_v4().to_string(),
        "audit",
        "channel.poll_lifecycle",
        OLD_US,
        &envelope("channel.poll_lifecycle", "allow").to_string(),
        false,
    );
    // At exactly the cutoff, and one microsecond before it.
    fixture.generic("whoami", CUTOFF_US);
    let just_old = fixture.generic("whoami", CUTOFF_US - 1);
    // Non-generic envelopes.
    let mut no_gate = envelope("comm.probe", "allow");
    no_gate.as_object_mut().unwrap().remove("gate_impl");
    fixture.insert(
        &Uuid::new_v4().to_string(),
        "audit",
        "comm.probe",
        OLD_US,
        &no_gate.to_string(),
        false,
    );
    fixture.insert(
        &Uuid::new_v4().to_string(),
        "audit",
        "comm.probe",
        OLD_US,
        &envelope("comm.probe", "maybe").to_string(),
        false,
    );
    fixture.insert(
        &Uuid::new_v4().to_string(),
        "audit",
        "comm.probe",
        OLD_US,
        &envelope("comm.inbox", "allow").to_string(),
        false,
    );
    // Aggregate identity, a payload that is not an object, a non-canonical id. The events schema indexes
    // json_extract(payload, ...), so SQLite refuses unparseable text at insert; valid non-object JSON is
    // the malformed shape a store with this schema can hold.
    fixture.insert(
        &Uuid::new_v4().to_string(),
        "audit",
        "comm.heartbeat",
        OLD_US,
        &envelope("comm.heartbeat", "allow").to_string(),
        true,
    );
    fixture.insert(
        &Uuid::new_v4().to_string(),
        "audit",
        "comm.heartbeat",
        OLD_US,
        r#"["not an object"]"#,
        false,
    );
    fixture.insert(
        &Uuid::new_v4().to_string().to_uppercase(),
        "audit",
        "comm.heartbeat",
        OLD_US,
        &envelope("comm.heartbeat", "allow").to_string(),
        false,
    );
    // Retention stamps: the producer's exact one is selected, any other retains.
    let mut stamped = envelope("comm.cursor_get", "deny");
    stamped["retention"] =
        json!({"class": "expendable", "policy": "poll-dispatch-v1", "horizon_seconds": 604800});
    let stamped_id = Uuid::new_v4().to_string();
    fixture.insert(
        &stamped_id,
        "audit",
        "comm.cursor_get",
        OLD_US,
        &stamped.to_string(),
        false,
    );
    let mut other_stamp = envelope("comm.cursor_get", "allow");
    other_stamp["retention"] =
        json!({"class": "expendable", "policy": "poll-dispatch-v2", "horizon_seconds": 604800});
    fixture.insert(
        &Uuid::new_v4().to_string(),
        "audit",
        "comm.cursor_get",
        OLD_US,
        &other_stamp.to_string(),
        false,
    );
    // A sampled row (ADR-103 Amendment 7) is still a generic envelope.
    let mut sampled = envelope("comm.cleanup_expired_quarantine", "gate_unavailable");
    sampled["resource"] = json!({"sampling": {"rate": 0.01, "forced": false}});
    let sampled_id = Uuid::new_v4().to_string();
    fixture.insert(
        &sampled_id,
        "audit",
        "comm.cleanup_expired_quarantine",
        OLD_US,
        &sampled.to_string(),
        false,
    );
    // Observations, and edges: live, soft-deleted, and naming the id in upper case.
    let observed = fixture.generic("comm.inbox", OLD_US);
    fixture
        .conn()
        .execute(
            "INSERT INTO event_observations (event_id, entity_id, referent_kind, role, position) VALUES (?1, 'e', 'entity', 'target', 0)",
            [&observed],
        )
        .expect("observation");
    let live_edge = fixture.generic("comm.inbox", OLD_US);
    fixture.edge(&live_edge, &Uuid::new_v4().to_string(), false);
    let deleted_edge = fixture.generic("comm.inbox", OLD_US);
    fixture.edge(&Uuid::new_v4().to_string(), &deleted_edge, true);
    let upper_edge = fixture.generic("comm.inbox", OLD_US);
    fixture.edge(
        &upper_edge.to_uppercase(),
        &Uuid::new_v4().to_string(),
        false,
    );

    let plan = plan(&fixture);
    let selected: BTreeSet<String> = plan
        .selected
        .iter()
        .map(|c| c.id.hyphenated().to_string())
        .collect();
    let expected: BTreeSet<String> = [keep, just_old, stamped_id, sampled_id]
        .into_iter()
        .collect();
    assert_eq!(selected, expected);
    let retained: Vec<(&str, u64)> = plan
        .retained_by_reason
        .iter()
        .map(|(k, v)| (k.as_str(), *v))
        .collect();
    assert_eq!(
        retained,
        vec![
            ("aggregate", 1),
            ("edge_referenced", 3),
            ("malformed", 2),
            ("non_generic_envelope", 3),
            ("observations_present", 1),
            ("retention_stamp_mismatch", 1),
            ("younger", 1),
        ]
    );
    assert_eq!(plan.protected_backlog, 11);
    let keys: Vec<(&str, &str)> = plan
        .non_candidate_keys
        .iter()
        .map(|k| (k.kind.as_str(), k.verb.as_str()))
        .collect();
    assert_eq!(
        keys,
        vec![
            ("audit", "channel.poll_lifecycle"),
            ("audit", "knowledge.get"),
            ("phase_started", "comm.inbox")
        ]
    );
}

#[test]
fn the_digest_ignores_order_and_duplicates_and_matches_canonical_json() {
    let ids: Vec<Uuid> = (0..20).map(|_| Uuid::new_v4()).collect();
    let mut shuffled = ids.clone();
    shuffled.reverse();
    shuffled.extend(ids.iter().take(5).copied());
    assert_eq!(
        events_prune::candidate_digest(ids.clone()),
        events_prune::candidate_digest(shuffled)
    );
    let mut strings: Vec<String> = ids.iter().map(|id| id.hyphenated().to_string()).collect();
    strings.sort();
    let canonical = khive_types::canonical_json_bytes(&json!(strings)).expect("canonical json");
    assert_eq!(
        events_prune::candidate_digest(ids),
        blake3::hash(&canonical).to_hex().to_string()
    );
    assert_eq!(
        events_prune::candidate_digest(Vec::new()),
        blake3::hash(b"[]").to_hex().to_string()
    );
}

#[test]
fn a_dry_run_changes_no_byte_of_either_store() {
    let fixture = Fixture::new();
    fixture.generic("comm.inbox", OLD_US);
    fixture.checkpoint();
    let before = (
        std::fs::read(&fixture.events).unwrap(),
        std::fs::read(&fixture.main).unwrap(),
    );
    let plan = plan(&fixture);
    assert_eq!(plan.candidates, 1);
    let after = (
        std::fs::read(&fixture.events).unwrap(),
        std::fs::read(&fixture.main).unwrap(),
    );
    assert!(before == after, "a dry run changed a store");
}

#[test]
fn an_unknown_events_schema_is_refused_and_left_unchanged() {
    let fixture = Fixture::new();
    fixture
        .conn()
        .execute_batch("ALTER TABLE events ADD COLUMN future TEXT")
        .expect("alter");
    fixture.checkpoint();
    let before = std::fs::read(&fixture.events).unwrap();
    let error = events_prune::plan_at(&fixture.target(), RUN_STARTED_US, CUTOFF_US).unwrap_err();
    assert!(matches!(error, PruneError::Schema(_)), "{error}");
    assert_eq!(error.exit_code(), 3);
    assert_eq!(std::fs::read(&fixture.events).unwrap(), before);
}

#[test]
fn a_cutoff_later_than_the_horizon_is_refused() {
    let fixture = Fixture::new();
    let error =
        events_prune::plan_at(&fixture.target(), RUN_STARTED_US, CUTOFF_US + 1).unwrap_err();
    assert!(matches!(error, PruneError::Invalid(_)), "{error}");
}

#[test]
fn apply_removes_exactly_the_candidates_and_leaves_every_other_row_identical() {
    let fixture = Fixture::new();
    let candidates: BTreeSet<String> = (0..5)
        .map(|i| fixture.generic(EXPENDABLE_VERBS[i % 6], OLD_US - i as i64))
        .collect();
    fixture.generic("comm.inbox", CUTOFF_US);
    fixture.insert(
        &Uuid::new_v4().to_string(),
        "audit",
        "knowledge.get",
        OLD_US,
        &envelope("knowledge.get", "allow").to_string(),
        false,
    );
    fixture.insert(
        &Uuid::new_v4().to_string(),
        "feedback_explicit",
        "brain.feedback",
        OLD_US,
        "{}",
        false,
    );
    let referenced = fixture.generic("comm.inbox", OLD_US);
    fixture.edge(&referenced, &Uuid::new_v4().to_string(), false);
    let others_before = fixture.rows_except(&candidates);
    let schema_before = (schema_sql(&fixture.events), schema_sql(&fixture.main));

    let plan = plan(&fixture);
    assert_eq!(plan.candidates, 5);
    let receipt = fixture.backup(&plan, "backup.db");
    let report = events_prune::apply_at(
        &fixture.target(),
        &fixture.options(&plan.digest, receipt, "state"),
        RUN_STARTED_US + 1,
    )
    .expect("apply");
    assert_eq!(report.removed, 5);
    assert_eq!(report.batches, 3);
    assert_eq!(fixture.rows_except(&BTreeSet::new()), others_before);
    assert!(fixture.ids().is_disjoint(&candidates));
    assert_eq!(report.controls_before, report.controls_after);
    assert_eq!(report.candidate_key_rows_before_cutoff, [6, 1]);
    assert_eq!(
        (schema_sql(&fixture.events), schema_sql(&fixture.main)),
        schema_before
    );
    let ledger = std::fs::read_to_string(&report.expended_ids_file).expect("ledger");
    assert_eq!(
        ledger
            .lines()
            .filter(|l| l.contains("\"type\":\"intent\""))
            .count(),
        3
    );
    assert_eq!(
        ledger
            .lines()
            .filter(|l| l.contains("\"type\":\"commit\""))
            .count(),
        3
    );
    for id in &candidates {
        assert!(ledger.contains(id.as_str()), "{id} missing from the ledger");
    }
    // The backup still holds every removed row.
    let copy = Connection::open(fixture.root.join("backup.db")).expect("backup copy");
    let in_copy: i64 = copy
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(in_copy, 9);
}

#[test]
fn apply_without_a_backup_refuses_before_any_write() {
    let fixture = Fixture::new();
    fixture.generic("comm.inbox", OLD_US);
    let plan = plan(&fixture);
    let before = fixture.ids();
    let error = events_prune::apply_at(
        &fixture.target(),
        &fixture.options(
            &plan.digest,
            fixture.root.join("absent.receipt.json"),
            "state",
        ),
        RUN_STARTED_US + 1,
    )
    .unwrap_err();
    assert!(matches!(error, PruneError::NoBackup(_)), "{error}");
    assert_eq!(error.exit_code(), 5);
    assert_eq!(fixture.ids(), before);
    // A receipt for another digest is no backup for this one.
    let receipt = fixture.backup(&plan, "backup.db");
    let error = events_prune::apply_at(
        &fixture.target(),
        &fixture.options(&"0".repeat(64), receipt, "state2"),
        RUN_STARTED_US + 1,
    )
    .unwrap_err();
    assert!(matches!(error, PruneError::NoBackup(_)), "{error}");
    assert_eq!(fixture.ids(), before);
}

#[test]
fn a_changed_candidate_set_refuses_with_a_digest_mismatch_before_any_write() {
    let fixture = Fixture::new();
    let first = fixture.generic("comm.inbox", OLD_US);
    fixture.generic("whoami", OLD_US);
    let plan = plan(&fixture);
    let receipt = fixture.backup(&plan, "backup.db");
    fixture.edge(&first, &Uuid::new_v4().to_string(), false);
    let before = fixture.ids();
    let error = events_prune::apply_at(
        &fixture.target(),
        &fixture.options(&plan.digest, receipt, "state"),
        RUN_STARTED_US + 1,
    )
    .unwrap_err();
    assert!(
        matches!(error, PruneError::DigestMismatch { .. }),
        "{error}"
    );
    assert_eq!(error.exit_code(), 4);
    assert_eq!(fixture.ids(), before);
}

#[test]
fn an_interrupted_apply_resumes_to_the_same_removed_set() {
    let build = || {
        let fixture = Fixture::new();
        for i in 0..7 {
            let id = format!("00000000-0000-4000-8000-00000000000{i}");
            fixture.insert(
                &id,
                "audit",
                "comm.probe",
                OLD_US - i,
                &envelope("comm.probe", "allow").to_string(),
                false,
            );
        }
        let survivor = "00000000-0000-4000-8000-0000000000ff";
        fixture.insert(
            survivor,
            "audit",
            "comm.probe",
            CUTOFF_US,
            &envelope("comm.probe", "allow").to_string(),
            false,
        );
        fixture
    };
    let whole = build();
    let plan_whole = plan(&whole);
    let receipt = whole.backup(&plan_whole, "backup.db");
    events_prune::apply_at(
        &whole.target(),
        &whole.options(&plan_whole.digest, receipt, "state"),
        RUN_STARTED_US + 1,
    )
    .expect("uninterrupted apply");

    let parts = build();
    let plan_parts = plan(&parts);
    assert_eq!(plan_parts.digest, plan_whole.digest);
    let receipt = parts.backup(&plan_parts, "backup.db");
    let mut options = parts.options(&plan_parts.digest, receipt, "state");
    options.max_batches = Some(1);
    let error = events_prune::apply_at(&parts.target(), &options, RUN_STARTED_US + 1).unwrap_err();
    assert!(matches!(error, PruneError::Interrupted(_)), "{error}");
    assert_eq!(error.exit_code(), 7);
    assert_eq!(parts.ids().len(), 6);

    // A run killed after its delete committed and before its commit line: the intent line is all that remains.
    let next: Vec<String> = (2..4)
        .map(|i| format!("00000000-0000-4000-8000-00000000000{}", 6 - i))
        .collect();
    let ledger = options.state_dir.join("expended-ids.ndjson");
    let intent = json!({
        "type": "intent",
        "digest": plan_parts.digest,
        "batch": 2,
        "ids": next.iter().map(|id| json!({"id": id, "verb": "comm.probe", "namespace": "local", "created_at": 0})).collect::<Vec<_>>()
    });
    let mut text = std::fs::read_to_string(&ledger).unwrap();
    text.push_str(&format!("{intent}\n"));
    std::fs::write(&ledger, text).unwrap();
    for id in &next {
        parts
            .conn()
            .execute("DELETE FROM events WHERE id = ?1", [id])
            .unwrap();
    }

    options.max_batches = None;
    let report = events_prune::apply_at(&parts.target(), &options, RUN_STARTED_US + 2)
        .expect("resumed apply");
    assert_eq!(report.reconciled, 2);
    assert_eq!(report.removed_total, 7);
    assert_eq!(parts.ids(), whole.ids());
}

#[test]
fn a_row_an_edge_names_after_selection_survives_its_batch() {
    let fixture = Fixture::new();
    let a = fixture.generic("comm.inbox", OLD_US);
    let b = fixture.generic("comm.inbox", OLD_US);
    let ids: Vec<events_prune::LedgerId> = [&a, &b]
        .iter()
        .map(|id| events_prune::LedgerId {
            id: (*id).clone(),
            verb: "comm.inbox".into(),
            namespace: "local".into(),
            created_at: OLD_US,
        })
        .collect();
    let edges: std::collections::HashSet<Uuid> =
        [Uuid::parse_str(&b).unwrap()].into_iter().collect();
    let conn = fixture.conn();
    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
    let (removed, skipped) =
        events_prune::delete_batch(&conn, &ids, CUTOFF_US, &edges).expect("batch");
    conn.execute_batch("COMMIT").unwrap();
    assert_eq!(removed, vec![a]);
    assert_eq!(skipped, vec![(b.clone(), "edge_referenced".to_string())]);
    assert!(fixture.ids().contains(&b));
}

#[test]
fn a_falling_control_count_stops_the_run() {
    let key = |rows_before_cutoff, rows| KeyCount {
        kind: "feedback_explicit".into(),
        verb: "brain.feedback".into(),
        rows_before_cutoff,
        rows,
    };
    assert!(events_prune::check_controls(&[key(3, 5)], &[key(3, 6)], [10, 8], 2).is_ok());
    let error = events_prune::check_controls(&[key(3, 5)], &[key(2, 5)], [10, 8], 2).unwrap_err();
    assert_eq!(error.exit_code(), 6);
    assert!(events_prune::check_controls(&[key(3, 5)], &[], [10, 8], 2).is_err());
    assert!(events_prune::check_controls(&[key(3, 5)], &[key(3, 5)], [10, 9], 2).is_err());
}

#[test]
fn a_backup_is_verified_and_never_overwrites() {
    let fixture = Fixture::new();
    fixture.generic("comm.inbox", OLD_US);
    fixture.insert(
        &Uuid::new_v4().to_string(),
        "feedback_explicit",
        "brain.feedback",
        OLD_US,
        "{}",
        false,
    );
    let plan = plan(&fixture);
    let dest = fixture.root.join("backup.db");
    let receipt = events_prune::backup(&fixture.target(), &plan, &dest).expect("backup");
    assert_eq!(receipt.integrity, vec!["ok".to_string()]);
    assert_eq!(receipt.digest, plan.digest);
    assert_eq!(receipt.rows_by_kind.values().sum::<u64>(), 2);
    assert_eq!(receipt.bytes, std::fs::metadata(&dest).unwrap().len());
    assert!(
        !fixture.root.join("backup.db-wal").exists(),
        "the copy is one self-contained file"
    );
    let error = events_prune::backup(&fixture.target(), &plan, &dest).unwrap_err();
    assert!(matches!(error, PruneError::Invalid(_)), "{error}");
    // A copy that no longer matches its receipt is no backup.
    std::fs::write(&dest, b"changed").unwrap();
    let error = events_prune::apply_at(
        &fixture.target(),
        &fixture.options(&plan.digest, events_prune::receipt_path(&dest), "state"),
        RUN_STARTED_US + 1,
    )
    .unwrap_err();
    assert!(matches!(error, PruneError::NoBackup(_)), "{error}");
}
