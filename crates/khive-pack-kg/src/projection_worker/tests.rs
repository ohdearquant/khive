use super::*;
use khive_runtime::{KhiveRuntime, Namespace};
use khive_storage::event::Event;
use khive_types::{EventKind, Id128, ProposalDecision, SubstrateKind};
use uuid::Uuid;

fn setup() -> (KhiveRuntime, NamespaceToken) {
    let rt = KhiveRuntime::memory().expect("in-memory runtime");
    let tok = rt.authorize(Namespace::local()).unwrap();
    (rt, tok)
}

async fn ensure_schema(rt: &KhiveRuntime) {
    let sql = rt.sql();
    let mut writer = sql.writer().await.expect("writer");
    writer
        .execute(SqlStatement {
            sql: "\
            CREATE TABLE IF NOT EXISTS proposals_open (\
                proposal_id TEXT PRIMARY KEY, \
                namespace TEXT NOT NULL, \
                proposer TEXT NOT NULL, \
                title TEXT NOT NULL, \
                status TEXT NOT NULL, \
                created_at INTEGER NOT NULL, \
                updated_at INTEGER NOT NULL, \
                expiry INTEGER, \
                last_decision TEXT, \
                review_count INTEGER NOT NULL DEFAULT 0, \
                approve_count INTEGER NOT NULL DEFAULT 0, \
                reject_count INTEGER NOT NULL DEFAULT 0\
            )"
            .to_string(),
            params: vec![],
            label: Some("test.ensure_schema".into()),
        })
        .await
        .expect("create table");
}

fn proposal_applied_event(token: &NamespaceToken, proposal_id: Uuid) -> Event {
    let mut event = Event::new(
        token.namespace().as_str(),
        "propose-apply",
        EventKind::ProposalApplied,
        SubstrateKind::Entity,
        "system:propose-apply",
    );
    event.payload = serde_json::json!({ "proposal_id": proposal_id });
    event.aggregate_kind = Some("proposal".to_string());
    event.aggregate_id = Some(proposal_id);
    event
}

async fn event_exists(rt: &KhiveRuntime, event_id: Uuid) -> bool {
    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    reader
        .query_row(SqlStatement {
            sql: "SELECT 1 AS present FROM events WHERE id = ?1".to_string(),
            params: vec![SqlValue::Text(event_id.to_string())],
            label: Some("test.proposal_applied_event_exists".into()),
        })
        .await
        .expect("query event")
        .is_some()
}

#[tokio::test]
async fn on_proposal_created_inserts_open_row() {
    let (rt, tok) = setup();
    ensure_schema(&rt).await;
    let worker = ProposalsProjectionWorker::new(rt.clone());
    let pid = Uuid::new_v4();

    worker
        .on_proposal_created(&tok, pid, "alice", "Add RoPE", None)
        .await
        .expect("on_proposal_created must succeed");

    let row = worker
        .get_proposal_row(&tok, pid)
        .await
        .expect("get_proposal_row must succeed")
        .expect("row must exist");

    assert_eq!(row.status, "open");
    assert_eq!(row.proposer, "alice");
}

#[tokio::test]
async fn on_proposal_reviewed_approve_sets_status_approved() {
    let (rt, tok) = setup();
    ensure_schema(&rt).await;
    let worker = ProposalsProjectionWorker::new(rt.clone());
    let pid = Uuid::new_v4();

    worker
        .on_proposal_created(&tok, pid, "alice", "Test Proposal", None)
        .await
        .expect("create");

    let payload = ProposalReviewedPayload {
        proposal_id: Id128::from_u128(pid.as_u128()),
        reviewer: "bob".to_string(),
        decision: ProposalDecision::Approve,
        comment: None,
    };
    worker
        .on_proposal_reviewed(&tok, &payload)
        .await
        .expect("on_proposal_reviewed must succeed");

    let row = worker
        .get_proposal_row(&tok, pid)
        .await
        .expect("get row")
        .expect("row must exist");

    assert_eq!(row.status, "approved");
    assert_eq!(row.approve_count, 1);
    assert_eq!(row.reject_count, 0);
}

#[tokio::test]
async fn on_proposal_withdrawn_sets_status_withdrawn() {
    let (rt, tok) = setup();
    ensure_schema(&rt).await;
    let worker = ProposalsProjectionWorker::new(rt.clone());
    let pid = Uuid::new_v4();

    worker
        .on_proposal_created(&tok, pid, "alice", "Withdraw Me", None)
        .await
        .expect("create");

    worker
        .on_proposal_withdrawn(&tok, pid)
        .await
        .expect("on_proposal_withdrawn must succeed");

    let row = worker
        .get_proposal_row(&tok, pid)
        .await
        .expect("get row")
        .expect("row must exist");

    assert_eq!(row.status, "withdrawn");
}

#[tokio::test]
async fn applied_and_emit_sets_status_applied_and_publishes_success() {
    let (rt, tok) = setup();
    ensure_schema(&rt).await;
    let worker = ProposalsProjectionWorker::new(rt.clone());
    let pid = Uuid::new_v4();

    worker
        .on_proposal_created(&tok, pid, "alice", "Apply Me", None)
        .await
        .expect("create");

    // Simulate approve path: approved → applying (pre-apply CAS) → applied.
    let approve_payload = ProposalReviewedPayload {
        proposal_id: Id128::from_u128(pid.as_u128()),
        reviewer: "alice".to_string(),
        decision: ProposalDecision::Approve,
        comment: None,
    };
    worker
        .on_proposal_reviewed(&tok, &approve_payload)
        .await
        .expect("review");

    // H1: pre_apply_cas must succeed when status='approved'.
    let claimed = worker
        .pre_apply_cas(&tok, pid)
        .await
        .expect("pre_apply_cas must succeed");
    assert!(
        claimed,
        "pre_apply_cas must return true when status='approved'"
    );

    let mut event = proposal_applied_event(&tok, pid);
    event.namespace = "forged-namespace".to_string();
    event.actor = "forged-actor".to_string();
    let event_id = event.id;
    let usage = khive_runtime::usage::UsageContext::new();
    let applied = khive_runtime::usage::scope(usage.clone(), async {
        worker.applied_and_emit(&tok, pid, event).await
    })
    .await
    .expect("applied_and_emit must succeed");
    assert!(applied, "CAS must succeed when status='applying'");
    assert_eq!(
        usage.snapshot()["event_rows"],
        1,
        "the raw atomic INSERT must preserve EventStore append accounting"
    );

    let row = worker
        .get_proposal_row(&tok, pid)
        .await
        .expect("get row")
        .expect("row must exist");

    assert_eq!(row.status, "applied");
    assert!(
        event_exists(&rt, event_id).await,
        "ProposalApplied success must be published with the applied projection"
    );
    let persisted = rt
        .events(&tok)
        .expect("event store")
        .get_event(event_id)
        .await
        .expect("read event")
        .expect("event exists");
    assert_eq!(persisted.namespace, "local");
    assert_eq!(persisted.actor, "anonymous:local");
}

// H1 regression: pre_apply_cas must fail when proposal was already withdrawn.
#[tokio::test]
async fn pre_apply_cas_fails_when_already_withdrawn() {
    let (rt, tok) = setup();
    ensure_schema(&rt).await;
    let worker = ProposalsProjectionWorker::new(rt.clone());
    let pid = Uuid::new_v4();

    worker
        .on_proposal_created(&tok, pid, "alice", "Race Test", None)
        .await
        .expect("create");

    let approve_payload = ProposalReviewedPayload {
        proposal_id: Id128::from_u128(pid.as_u128()),
        reviewer: "bob".to_string(),
        decision: ProposalDecision::Approve,
        comment: None,
    };
    worker
        .on_proposal_reviewed(&tok, &approve_payload)
        .await
        .expect("approve");

    worker
        .on_proposal_withdrawn(&tok, pid)
        .await
        .expect("withdraw");

    // pre_apply_cas must fail: status is 'withdrawn', not 'approved'.
    let claimed = worker
        .pre_apply_cas(&tok, pid)
        .await
        .expect("pre_apply_cas must not error");
    assert!(
        !claimed,
        "H1: pre_apply_cas must return false when status='withdrawn'"
    );

    let row = worker
        .get_proposal_row(&tok, pid)
        .await
        .expect("get row")
        .expect("row must exist");
    assert_eq!(
        row.status, "withdrawn",
        "status must remain 'withdrawn' after failed pre_apply_cas"
    );
}

// H1 regression: on_proposal_withdrawn must fail when status='applying'.
#[tokio::test]
async fn on_proposal_withdrawn_fails_when_status_applying() {
    let (rt, tok) = setup();
    ensure_schema(&rt).await;
    let worker = ProposalsProjectionWorker::new(rt.clone());
    let pid = Uuid::new_v4();

    worker
        .on_proposal_created(&tok, pid, "alice", "Applying Guard", None)
        .await
        .expect("create");

    let approve_payload = ProposalReviewedPayload {
        proposal_id: Id128::from_u128(pid.as_u128()),
        reviewer: "bob".to_string(),
        decision: ProposalDecision::Approve,
        comment: None,
    };
    worker
        .on_proposal_reviewed(&tok, &approve_payload)
        .await
        .expect("approve");

    // Simulate apply worker claiming 'applying'.
    let claimed = worker
        .pre_apply_cas(&tok, pid)
        .await
        .expect("pre_apply_cas");
    assert!(claimed, "pre_apply_cas must succeed");

    // Now withdraw must be blocked (status='applying').
    let withdrew = worker
        .on_proposal_withdrawn(&tok, pid)
        .await
        .expect("on_proposal_withdrawn must not error");
    assert!(
        !withdrew,
        "H1: on_proposal_withdrawn must return false when status='applying'"
    );

    // Status must still be 'applying'.
    let row = worker
        .get_proposal_row(&tok, pid)
        .await
        .expect("get row")
        .expect("row must exist");
    assert_eq!(
        row.status, "applying",
        "status must remain 'applying' after blocked withdraw"
    );
}

// BUG-3 regression: on_proposal_reviewed must store the bare variant name in
// last_decision, not the JSON-quoted form "\"approve\"".
#[tokio::test]
async fn on_proposal_reviewed_last_decision_is_bare_string() {
    let (rt, tok) = setup();
    ensure_schema(&rt).await;
    let worker = ProposalsProjectionWorker::new(rt.clone());
    let pid = Uuid::new_v4();

    worker
        .on_proposal_created(&tok, pid, "alice", "Encoding Test", None)
        .await
        .expect("create");

    for (decision, expected_str) in [
        (ProposalDecision::Approve, "approve"),
        (ProposalDecision::Reject, "reject"),
        (ProposalDecision::Comment, "comment"),
        (ProposalDecision::RequestChanges, "request_changes"),
    ] {
        // Reset for each variant.
        let pid2 = Uuid::new_v4();
        worker
            .on_proposal_created(&tok, pid2, "alice", "Encoding Test", None)
            .await
            .expect("create");

        let payload = ProposalReviewedPayload {
            proposal_id: Id128::from_u128(pid2.as_u128()),
            reviewer: "bob".to_string(),
            decision,
            comment: None,
        };
        worker
            .on_proposal_reviewed(&tok, &payload)
            .await
            .expect("on_proposal_reviewed must succeed");

        // Read the raw last_decision column.
        let sql = rt.sql();
        let mut reader = sql.reader().await.expect("reader");
        let row = reader
            .query_row(SqlStatement {
                sql: "SELECT last_decision FROM proposals_open WHERE proposal_id = ?1".to_string(),
                params: vec![SqlValue::Text(pid2.to_string())],
                label: Some("test.last_decision_encoding".into()),
            })
            .await
            .expect("query_row")
            .expect("row must exist");

        let stored = row
            .get("last_decision")
            .and_then(|v| {
                if let SqlValue::Text(s) = v {
                    Some(s.clone())
                } else {
                    None
                }
            })
            .unwrap_or_default();

        assert_eq!(
            stored, expected_str,
            "BUG-3: last_decision for {decision:?} must be bare {expected_str:?}, not JSON-quoted; got: {stored:?}"
        );
        assert!(
            !stored.starts_with('"'),
            "BUG-3: last_decision must NOT be JSON-quoted; got: {stored:?}"
        );
    }
}

// BUG-4 regression: second on_proposal_withdrawn on an already-withdrawn
// proposal returns false (CAS missed).
#[tokio::test]
async fn on_proposal_withdrawn_cas_returns_false_on_second_call() {
    let (rt, tok) = setup();
    ensure_schema(&rt).await;
    let worker = ProposalsProjectionWorker::new(rt.clone());
    let pid = Uuid::new_v4();

    worker
        .on_proposal_created(&tok, pid, "alice", "Withdraw Race", None)
        .await
        .expect("create");

    let first = worker
        .on_proposal_withdrawn(&tok, pid)
        .await
        .expect("first withdraw must not error");
    assert!(first, "BUG-4: first on_proposal_withdrawn must return true");

    let second = worker
        .on_proposal_withdrawn(&tok, pid)
        .await
        .expect("second withdraw must not error");
    assert!(
        !second,
        "BUG-4: second on_proposal_withdrawn must return false (CAS missed)"
    );
}

// H1 regression: two sequential `withdrawn_and_emit` calls on the same
// open proposal must produce exactly ONE ProposalWithdrawn event in the events
// table, and the second call must return cas_hit=false.
#[tokio::test]
async fn withdrawn_and_emit_second_call_no_duplicate_event() {
    use khive_storage::event::Event;
    use khive_types::{EventKind, SubstrateKind};

    let (rt, tok) = setup();
    ensure_schema(&rt).await;
    let worker = ProposalsProjectionWorker::new(rt.clone());
    let pid = Uuid::new_v4();

    worker
        .on_proposal_created(&tok, pid, "alice", "Duplicate Guard Test", None)
        .await
        .expect("create");

    let make_event = || {
        Event::new(
            tok.namespace().as_str(),
            "withdraw",
            EventKind::ProposalWithdrawn,
            SubstrateKind::Note,
            "alice",
        )
    };

    // First withdraw — must succeed.
    let (cas1, _eid1) = worker
        .withdrawn_and_emit(&tok, pid, make_event())
        .await
        .expect("first withdrawn_and_emit must not error");
    assert!(cas1, "first withdrawn_and_emit must return cas_hit=true");

    // Second withdraw — CAS misses (status already 'withdrawn'), event must NOT be inserted.
    let (cas2, _eid2) = worker
        .withdrawn_and_emit(&tok, pid, make_event())
        .await
        .expect("second withdrawn_and_emit must not error");
    assert!(
        !cas2,
        "H1-R3: second withdrawn_and_emit must return cas_hit=false"
    );

    // Critical: exactly ONE ProposalWithdrawn event in the events table.
    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT id FROM events WHERE kind='proposal_withdrawn' AND aggregate_id IS NULL AND target_id IS NULL".to_string(),
            params: vec![],
            label: Some("test.count_withdrawn_events".into()),
        })
        .await
        .expect("query_all");

    // Count all ProposalWithdrawn events — there must be exactly one.
    let withdrawn_count = {
        let sql2 = rt.sql();
        let mut reader2 = sql2.reader().await.expect("reader2");
        reader2
            .query_row(SqlStatement {
                sql: "SELECT COUNT(*) as cnt FROM events WHERE kind='proposal_withdrawn'"
                    .to_string(),
                params: vec![],
                label: Some("test.withdrawn_event_count".into()),
            })
            .await
            .expect("count query")
    };

    let count = withdrawn_count
        .and_then(|row| {
            row.get("cnt").and_then(|v| {
                if let SqlValue::Integer(n) = v {
                    Some(*n)
                } else {
                    None
                }
            })
        })
        .unwrap_or(0);

    assert_eq!(
        count, 1,
        "H1-R3: exactly ONE ProposalWithdrawn event must exist; got {count}. \
         Duplicate events indicate the guard checked final status instead of \
         whether this UPDATE actually ran."
    );
    drop(rows); // silence unused-variable lint
}

// Regression: same-microsecond `updated_at` collision.
#[tokio::test]
async fn same_microsecond_timestamp_no_duplicate_event_changes_guard() {
    let (rt, tok) = setup();
    ensure_schema(&rt).await;
    let ns = tok.namespace().as_str().to_owned();
    let pid = Uuid::new_v4();
    let pid_str = pid.to_string();

    let shared_now: i64 = 1_700_000_000_000_000;
    {
        let sql = rt.sql();
        let mut writer = sql.writer().await.expect("writer");
        writer
            .execute(SqlStatement {
                sql: "INSERT INTO proposals_open \
                        (proposal_id, namespace, proposer, title, status, \
                         created_at, updated_at) \
                      VALUES (?1, ?2, 'alice', 'Timestamp Race', 'open', ?3, ?3)"
                    .to_string(),
                params: vec![
                    SqlValue::Text(pid_str.clone()),
                    SqlValue::Text(ns.clone()),
                    SqlValue::Integer(shared_now - 1),
                ],
                label: Some("test.insert_open".into()),
            })
            .await
            .expect("insert proposal");
    }

    // Caller A: UPDATE + guarded INSERT.
    {
        let sql = rt.sql();
        let mut writer = sql.writer().await.expect("writer");
        let total = writer
            .execute_batch(vec![
                SqlStatement {
                    sql: "UPDATE proposals_open \
                          SET status = 'withdrawn', updated_at = ?1 \
                          WHERE proposal_id = ?2 AND namespace = ?3 \
                            AND status NOT IN ('applied', 'applying', 'withdrawn', 'rejected')"
                        .to_string(),
                    params: vec![
                        SqlValue::Integer(shared_now),
                        SqlValue::Text(pid_str.clone()),
                        SqlValue::Text(ns.clone()),
                    ],
                    label: Some("test.caller_a.update".into()),
                },
                SqlStatement {
                    sql: "INSERT INTO events \
                           (id, namespace, verb, substrate, actor, kind, outcome, payload, \
                            payload_schema_version, duration_us, created_at) \
                           SELECT ?1, ?2, 'withdraw', 'note', 'alice', \
                                  'proposal_withdrawn', 'ok', '{}', 1, 0, ?3 \
                           WHERE (changes() = 1)"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(Uuid::new_v4().to_string()),
                        SqlValue::Text(ns.clone()),
                        SqlValue::Integer(shared_now),
                    ],
                    label: Some("test.caller_a.insert_event".into()),
                },
            ])
            .await
            .expect("caller_a execute_batch");
        assert_eq!(
            total, 2,
            "caller A must write 1 UPDATE row + 1 event INSERT row; got {total}"
        );
    }

    // Caller B: same timestamp, UPDATE hits 0 rows.
    {
        let sql = rt.sql();
        let mut writer = sql.writer().await.expect("writer");
        let total = writer
            .execute_batch(vec![
                SqlStatement {
                    sql: "UPDATE proposals_open \
                          SET status = 'withdrawn', updated_at = ?1 \
                          WHERE proposal_id = ?2 AND namespace = ?3 \
                            AND status NOT IN ('applied', 'applying', 'withdrawn', 'rejected')"
                        .to_string(),
                    params: vec![
                        SqlValue::Integer(shared_now),
                        SqlValue::Text(pid_str.clone()),
                        SqlValue::Text(ns.clone()),
                    ],
                    label: Some("test.caller_b.update".into()),
                },
                SqlStatement {
                    sql: "INSERT INTO events \
                           (id, namespace, verb, substrate, actor, kind, outcome, payload, \
                            payload_schema_version, duration_us, created_at) \
                           SELECT ?1, ?2, 'withdraw', 'note', 'alice', \
                                  'proposal_withdrawn', 'ok', '{}', 1, 0, ?3 \
                           WHERE (changes() = 1)"
                        .to_string(),
                    params: vec![
                        SqlValue::Text(Uuid::new_v4().to_string()),
                        SqlValue::Text(ns.clone()),
                        SqlValue::Integer(shared_now),
                    ],
                    label: Some("test.caller_b.insert_event".into()),
                },
            ])
            .await
            .expect("caller_b execute_batch");
        assert_eq!(
            total, 0,
            "caller B's UPDATE hits 0 rows; changes() = 0 so INSERT must be skipped; \
             got {total} (an earlier guard would have returned 1 here — duplicate event)"
        );
    }

    // Verify: exactly ONE ProposalWithdrawn event.
    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let count_row = reader
        .query_row(SqlStatement {
            sql: "SELECT COUNT(*) as cnt FROM events WHERE kind='proposal_withdrawn'".to_string(),
            params: vec![],
            label: Some("test.same_micros.event_count".into()),
        })
        .await
        .expect("count query");
    let count = count_row
        .and_then(|row| {
            row.get("cnt").and_then(|v| {
                if let SqlValue::Integer(n) = v {
                    Some(*n)
                } else {
                    None
                }
            })
        })
        .unwrap_or(0);
    assert_eq!(
        count, 1,
        "R4: exactly ONE ProposalWithdrawn event must exist even with identical \
         `updated_at` timestamps; got {count}. \
         A value of 2 means the guard is incorrectly checking timestamp equality, \
         not connection-local changes()."
    );
}

// A missed finalization CAS must not publish a success event.
#[tokio::test]
async fn applied_and_emit_cas_miss_suppresses_success_event() {
    let (rt, tok) = setup();
    ensure_schema(&rt).await;
    let worker = ProposalsProjectionWorker::new(rt.clone());
    let pid = Uuid::new_v4();

    worker
        .on_proposal_created(&tok, pid, "alice", "Race Test", None)
        .await
        .expect("create");

    // Simulate approve then immediate withdraw.
    let approve_payload = ProposalReviewedPayload {
        proposal_id: Id128::from_u128(pid.as_u128()),
        reviewer: "bob".to_string(),
        decision: ProposalDecision::Approve,
        comment: None,
    };
    worker
        .on_proposal_reviewed(&tok, &approve_payload)
        .await
        .expect("approve");

    worker
        .on_proposal_withdrawn(&tok, pid)
        .await
        .expect("withdraw");

    let event = proposal_applied_event(&tok, pid);
    let event_id = event.id;
    let applied = worker
        .applied_and_emit(&tok, pid, event)
        .await
        .expect("applied_and_emit must not error");
    assert!(
        !applied,
        "applied_and_emit must return false when status is not 'applying'"
    );
    assert!(
        !event_exists(&rt, event_id).await,
        "a missed applied projection CAS must suppress ProposalApplied success"
    );

    // Verify the status did not flip back to 'applied'.
    let row = worker
        .get_proposal_row(&tok, pid)
        .await
        .expect("get row")
        .expect("row must exist");
    assert_eq!(
        row.status, "withdrawn",
        "status must remain 'withdrawn' after failed apply CAS"
    );
}

// A projection update error must roll back the batch before success is visible.
#[tokio::test]
async fn applied_and_emit_projection_error_suppresses_success_event() {
    let (rt, tok) = setup();
    ensure_schema(&rt).await;
    let worker = ProposalsProjectionWorker::new(rt.clone());
    let pid = Uuid::new_v4();

    worker
        .on_proposal_created(&tok, pid, "alice", "Projection Failure", None)
        .await
        .expect("create");
    let approve_payload = ProposalReviewedPayload {
        proposal_id: Id128::from_u128(pid.as_u128()),
        reviewer: "bob".to_string(),
        decision: ProposalDecision::Approve,
        comment: None,
    };
    worker
        .on_proposal_reviewed(&tok, &approve_payload)
        .await
        .expect("approve");
    assert!(
        worker
            .pre_apply_cas(&tok, pid)
            .await
            .expect("pre_apply_cas"),
        "precondition: proposal must be applying"
    );

    let sql = rt.sql();
    let mut writer = sql.writer().await.expect("writer");
    writer
        .execute_script(
            "CREATE TRIGGER fail_applied_projection \
             BEFORE UPDATE OF status ON proposals_open \
             WHEN NEW.status = 'applied' \
             BEGIN \
               SELECT RAISE(ABORT, 'forced applied projection failure'); \
             END"
            .to_string(),
        )
        .await
        .expect("install failure trigger");
    drop(writer);

    let event = proposal_applied_event(&tok, pid);
    let event_id = event.id;
    worker
        .applied_and_emit(&tok, pid, event)
        .await
        .expect_err("forced projection failure must surface");

    let row = worker
        .get_proposal_row(&tok, pid)
        .await
        .expect("get row")
        .expect("row must exist");
    assert_eq!(
        row.status, "applying",
        "failed finalization must leave the projection uncommitted"
    );
    assert!(
        !event_exists(&rt, event_id).await,
        "a projection update error must suppress ProposalApplied success"
    );
}

async fn guarded_usage_setup(
    file_backed: bool,
) -> (KhiveRuntime, NamespaceToken, Option<std::path::PathBuf>) {
    let directory = file_backed.then(|| {
        let path = std::env::temp_dir().join(format!("khive-kg-guarded-usage-{}", Uuid::new_v4()));
        std::fs::create_dir(&path).expect("isolated projection workspace");
        path
    });
    let rt = KhiveRuntime::new(khive_runtime::RuntimeConfig {
        db_path: directory.as_ref().map(|path| path.join("projection.db")),
        packs: vec!["kg".into()],
        actor_id: Some("lambda:kg-query-receipts-fixture".into()),
        brain_profile: None,
        ..khive_runtime::RuntimeConfig::no_embeddings()
    })
    .expect("isolated attributed projection runtime");
    let tok = rt
        .authorize(Namespace::local())
        .expect("projection namespace");
    let mut builder = khive_runtime::VerbRegistryBuilder::new();
    builder.register(crate::KgPack::new(rt.clone()));
    let registry = builder.build().expect("projection schema registry");
    registry.apply_schema_plans(rt.backend());
    (rt, tok, directory)
}

fn cleanup_guarded_usage(
    rt: KhiveRuntime,
    tok: NamespaceToken,
    directory: Option<std::path::PathBuf>,
) {
    drop(tok);
    drop(rt);
    if let Some(path) = directory {
        std::fs::remove_dir_all(path).expect("remove isolated projection workspace");
    }
}

fn reviewed_usage_payload(pid: Uuid, decision: ProposalDecision) -> ProposalReviewedPayload {
    ProposalReviewedPayload {
        proposal_id: Id128::from_u128(pid.as_u128()),
        reviewer: "synthetic-reviewer".into(),
        decision,
        comment: None,
    }
}

fn guarded_usage_event(pid: Uuid, kind: EventKind) -> Event {
    let mut event = Event::new(
        "forged-namespace",
        "synthetic-proposal",
        kind,
        SubstrateKind::Entity,
        "forged-actor",
    );
    event.aggregate_kind = Some("proposal".into());
    event.aggregate_id = Some(pid);
    event.payload = serde_json::json!({"proposal_id": pid});
    event
}

fn counted_event_rows(usage: &khive_runtime::usage::UsageContext) -> u64 {
    usage
        .snapshot()
        .get("event_rows")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

async fn guarded_projection_snapshot(rt: &KhiveRuntime, pid: Uuid) -> serde_json::Value {
    let access = rt.sql();
    let mut reader = access.reader().await.expect("projection snapshot reader");
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT * FROM proposals_open WHERE proposal_id = ?1".into(),
            params: vec![SqlValue::Text(pid.to_string())],
            label: Some("test.guarded_projection_snapshot".into()),
        })
        .await
        .expect("snapshot query")
        .expect("projection exists");
    serde_json::to_value(row).expect("serialize exact projection snapshot")
}

async fn assert_guarded_event_stamp(rt: &KhiveRuntime, tok: &NamespaceToken, event_id: Uuid) {
    let event = rt
        .events(tok)
        .expect("attributed event store")
        .get_event(event_id)
        .await
        .expect("event read")
        .expect("committed event");
    assert_eq!(event.namespace, tok.namespace().as_str());
    assert_eq!(
        event.actor,
        format!("{}:{}", tok.actor().kind, tok.actor().id)
    );
}

#[tokio::test]
async fn guarded_review_and_withdraw_count_only_committed_events() {
    for file_backed in [false, true] {
        let (rt, tok, directory) = guarded_usage_setup(file_backed).await;
        {
            let worker = ProposalsProjectionWorker::new(rt.clone());
            let pid = Uuid::new_v4();
            worker
                .on_proposal_created(&tok, pid, "synthetic-proposer", "usage", None)
                .await
                .expect("create projection");
            let payload = reviewed_usage_payload(pid, ProposalDecision::Approve);
            let event = guarded_usage_event(pid, EventKind::ProposalReviewed);
            let event_id = event.id;
            let usage = khive_runtime::usage::UsageContext::new();
            let (hit, receipt) = khive_runtime::usage::scope(
                usage.clone(),
                worker.reviewed_and_emit(&tok, &payload, event, true),
            )
            .await
            .expect("review commits");
            assert!(hit);
            assert_eq!(receipt, event_id);
            assert_eq!(counted_event_rows(&usage), 1);
            assert_guarded_event_stamp(&rt, &tok, event_id).await;
            let row = worker
                .get_proposal_row(&tok, pid)
                .await
                .expect("read projection")
                .expect("projection");
            assert_eq!(row.status, "approved");
            assert_eq!(row.approve_count, 1);
            assert_eq!(row.reject_count, 0);

            let event = guarded_usage_event(pid, EventKind::ProposalWithdrawn);
            let event_id = event.id;
            let usage = khive_runtime::usage::UsageContext::new();
            let (hit, receipt) = khive_runtime::usage::scope(
                usage.clone(),
                worker.withdrawn_and_emit(&tok, pid, event),
            )
            .await
            .expect("withdraw commits");
            assert!(hit);
            assert_eq!(receipt, event_id);
            assert_eq!(counted_event_rows(&usage), 1);
            assert_guarded_event_stamp(&rt, &tok, event_id).await;
            assert_eq!(
                worker
                    .get_proposal_row(&tok, pid)
                    .await
                    .expect("read")
                    .expect("projection")
                    .status,
                "withdrawn"
            );
        }
        cleanup_guarded_usage(rt, tok, directory);
    }
}

#[tokio::test]
async fn guarded_comment_counts_real_insert_but_not_logical_cas_hit() {
    for file_backed in [false, true] {
        let (rt, tok, directory) = guarded_usage_setup(file_backed).await;
        {
            let worker = ProposalsProjectionWorker::new(rt.clone());
            let pid = Uuid::new_v4();
            let payload = reviewed_usage_payload(pid, ProposalDecision::Comment);
            let event = guarded_usage_event(pid, EventKind::ProposalReviewed);
            let event_id = event.id;
            let usage = khive_runtime::usage::UsageContext::new();
            let (hit, receipt) = khive_runtime::usage::scope(
                usage.clone(),
                worker.reviewed_and_emit(&tok, &payload, event, false),
            )
            .await
            .expect("missing comment retains logical success");
            assert!(
                hit,
                "comment's public cas_hit stays true even without a row"
            );
            assert_eq!(receipt, event_id);
            assert!(!event_exists(&rt, event_id).await);
            assert_eq!(counted_event_rows(&usage), 0);

            worker
                .on_proposal_created(&tok, pid, "synthetic-proposer", "comment", None)
                .await
                .expect("create projection");
            let before = guarded_projection_snapshot(&rt, pid).await;
            let event = guarded_usage_event(pid, EventKind::ProposalReviewed);
            let event_id = event.id;
            let usage = khive_runtime::usage::UsageContext::new();
            let (hit, _) = khive_runtime::usage::scope(
                usage.clone(),
                worker.reviewed_and_emit(&tok, &payload, event, false),
            )
            .await
            .expect("existing comment commits");
            assert!(hit);
            assert_eq!(counted_event_rows(&usage), 1);
            assert_guarded_event_stamp(&rt, &tok, event_id).await;
            assert_ne!(guarded_projection_snapshot(&rt, pid).await, before);
            assert_eq!(
                worker
                    .get_proposal_row(&tok, pid)
                    .await
                    .expect("read")
                    .expect("projection")
                    .status,
                "open"
            );
        }
        cleanup_guarded_usage(rt, tok, directory);
    }
}

#[tokio::test]
async fn guarded_terminal_cas_misses_do_not_write_or_count_events() {
    for file_backed in [false, true] {
        let (rt, tok, directory) = guarded_usage_setup(file_backed).await;
        {
            let worker = ProposalsProjectionWorker::new(rt.clone());
            let pid = Uuid::new_v4();
            worker
                .on_proposal_created(&tok, pid, "synthetic-proposer", "terminal", None)
                .await
                .expect("create");
            assert!(worker
                .on_proposal_withdrawn(&tok, pid)
                .await
                .expect("seed terminal status"));
            let before = guarded_projection_snapshot(&rt, pid).await;
            let event = guarded_usage_event(pid, EventKind::ProposalWithdrawn);
            let event_id = event.id;
            let usage = khive_runtime::usage::UsageContext::new();
            let (hit, _) = khive_runtime::usage::scope(
                usage.clone(),
                worker.withdrawn_and_emit(&tok, pid, event),
            )
            .await
            .expect("withdraw CAS miss");
            assert!(!hit);
            assert!(!event_exists(&rt, event_id).await);
            assert_eq!(counted_event_rows(&usage), 0);
            let payload = reviewed_usage_payload(pid, ProposalDecision::Reject);
            let event = guarded_usage_event(pid, EventKind::ProposalReviewed);
            let event_id = event.id;
            let usage = khive_runtime::usage::UsageContext::new();
            let (hit, _) = khive_runtime::usage::scope(
                usage.clone(),
                worker.reviewed_and_emit(&tok, &payload, event, true),
            )
            .await
            .expect("review CAS miss");
            assert!(!hit);
            assert!(!event_exists(&rt, event_id).await);
            assert_eq!(counted_event_rows(&usage), 0);
            let event = guarded_usage_event(pid, EventKind::ProposalApplied);
            let event_id = event.id;
            let usage = khive_runtime::usage::UsageContext::new();
            assert!(!khive_runtime::usage::scope(
                usage.clone(),
                worker.applied_and_emit(&tok, pid, event)
            )
            .await
            .expect("apply CAS miss"));
            assert!(!event_exists(&rt, event_id).await);
            assert_eq!(counted_event_rows(&usage), 0);
            assert_eq!(guarded_projection_snapshot(&rt, pid).await, before);
        }
        cleanup_guarded_usage(rt, tok, directory);
    }
}

#[tokio::test]
async fn guarded_apply_shared_executor_counts_exactly_one_event() {
    for file_backed in [false, true] {
        let (rt, tok, directory) = guarded_usage_setup(file_backed).await;
        {
            let worker = ProposalsProjectionWorker::new(rt.clone());
            let pid = Uuid::new_v4();
            worker
                .on_proposal_created(&tok, pid, "synthetic-proposer", "apply", None)
                .await
                .expect("create");
            worker
                .on_proposal_reviewed(
                    &tok,
                    &reviewed_usage_payload(pid, ProposalDecision::Approve),
                )
                .await
                .expect("approve");
            assert!(worker
                .pre_apply_cas(&tok, pid)
                .await
                .expect("claim applying"));
            let event = guarded_usage_event(pid, EventKind::ProposalApplied);
            let event_id = event.id;
            let usage = khive_runtime::usage::UsageContext::new();
            assert!(khive_runtime::usage::scope(
                usage.clone(),
                worker.applied_and_emit(&tok, pid, event)
            )
            .await
            .expect("apply commits"));
            assert_eq!(
                counted_event_rows(&usage),
                1,
                "apply must not retain its former second increment"
            );
            assert_guarded_event_stamp(&rt, &tok, event_id).await;
            assert_eq!(
                worker
                    .get_proposal_row(&tok, pid)
                    .await
                    .expect("read")
                    .expect("projection")
                    .status,
                "applied"
            );
        }
        cleanup_guarded_usage(rt, tok, directory);
    }
}

#[tokio::test]
async fn guarded_event_insert_failure_rolls_back_projection_and_usage() {
    for file_backed in [false, true] {
        let (rt, tok, directory) = guarded_usage_setup(file_backed).await;
        {
            let worker = ProposalsProjectionWorker::new(rt.clone());
            for kind in [
                EventKind::ProposalReviewed,
                EventKind::ProposalWithdrawn,
                EventKind::ProposalApplied,
            ] {
                let pid = Uuid::new_v4();
                worker
                    .on_proposal_created(&tok, pid, "synthetic-proposer", "rollback", None)
                    .await
                    .expect("create");
                if kind == EventKind::ProposalApplied {
                    worker
                        .on_proposal_reviewed(
                            &tok,
                            &reviewed_usage_payload(pid, ProposalDecision::Approve),
                        )
                        .await
                        .expect("approve");
                    assert!(worker.pre_apply_cas(&tok, pid).await.expect("claim"));
                }
                let event = guarded_usage_event(pid, kind);
                let event_id = event.id;
                rt.events(&tok)
                    .expect("event store")
                    .append_event(event.clone())
                    .await
                    .expect("seed duplicate event ID");
                let persisted_before = rt
                    .events(&tok)
                    .expect("event store")
                    .get_event(event_id)
                    .await
                    .expect("read event")
                    .expect("event");
                let before = guarded_projection_snapshot(&rt, pid).await;
                let usage = khive_runtime::usage::UsageContext::new();
                let result = khive_runtime::usage::scope(usage.clone(), async {
                    match kind {
                        EventKind::ProposalReviewed => worker
                            .reviewed_and_emit(
                                &tok,
                                &reviewed_usage_payload(pid, ProposalDecision::Approve),
                                event,
                                true,
                            )
                            .await
                            .map(|_| ()),
                        EventKind::ProposalWithdrawn => worker
                            .withdrawn_and_emit(&tok, pid, event)
                            .await
                            .map(|_| ()),
                        EventKind::ProposalApplied => {
                            worker.applied_and_emit(&tok, pid, event).await.map(|_| ())
                        }
                        _ => unreachable!(),
                    }
                })
                .await;
                assert!(
                    result.is_err(),
                    "real duplicate event insert must fail after the projection UPDATE"
                );
                assert_eq!(
                    counted_event_rows(&usage),
                    0,
                    "rolled-back append is not executed event usage"
                );
                assert_eq!(
                    guarded_projection_snapshot(&rt, pid).await,
                    before,
                    "status, timestamps and review counters roll back together"
                );
                let persisted_after = rt
                    .events(&tok)
                    .expect("event store")
                    .get_event(event_id)
                    .await
                    .expect("read event")
                    .expect("preexisting event remains");
                assert_eq!(
                    serde_json::to_value(persisted_after).unwrap(),
                    serde_json::to_value(persisted_before).unwrap()
                );
            }
        }
        cleanup_guarded_usage(rt, tok, directory);
    }
}
