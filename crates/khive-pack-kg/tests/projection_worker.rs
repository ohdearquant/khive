//! Integration tests for ProposalsProjectionWorker.
//!
//! Moved from `src/projection_worker.rs` inline tests (KG-AUD-004: inline test
//! sections must be under 300 lines).

use khive_pack_kg::projection_worker::ProposalsProjectionWorker;

use khive_runtime::{KhiveRuntime, Namespace, NamespaceToken};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_types::{Id128, ProposalDecision, ProposalReviewedPayload};
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
async fn on_proposal_applied_sets_status_applied() {
    let (rt, tok) = setup();
    ensure_schema(&rt).await;
    let worker = ProposalsProjectionWorker::new(rt.clone());
    let pid = Uuid::new_v4();

    worker
        .on_proposal_created(&tok, pid, "alice", "Apply Me", None)
        .await
        .expect("create");

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

    let claimed = worker
        .pre_apply_cas(&tok, pid)
        .await
        .expect("pre_apply_cas must succeed");
    assert!(
        claimed,
        "pre_apply_cas must return true when status='approved'"
    );

    let applied = worker
        .on_proposal_applied(&tok, pid)
        .await
        .expect("on_proposal_applied must succeed");
    assert!(applied, "CAS must succeed when status='applying'");

    let row = worker
        .get_proposal_row(&tok, pid)
        .await
        .expect("get row")
        .expect("row must exist");

    assert_eq!(row.status, "applied");
}

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

    let claimed = worker
        .pre_apply_cas(&tok, pid)
        .await
        .expect("pre_apply_cas");
    assert!(claimed, "pre_apply_cas must succeed");

    let withdrew = worker
        .on_proposal_withdrawn(&tok, pid)
        .await
        .expect("on_proposal_withdrawn must not error");
    assert!(
        !withdrew,
        "H1: on_proposal_withdrawn must return false when status='applying'"
    );

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

#[tokio::test]
async fn on_proposal_reviewed_last_decision_is_bare_string() {
    let (rt, tok) = setup();
    ensure_schema(&rt).await;
    let worker = ProposalsProjectionWorker::new(rt.clone());

    for (decision, expected_str) in [
        (ProposalDecision::Approve, "approve"),
        (ProposalDecision::Reject, "reject"),
        (ProposalDecision::Comment, "comment"),
        (ProposalDecision::RequestChanges, "request_changes"),
    ] {
        let pid = Uuid::new_v4();
        worker
            .on_proposal_created(&tok, pid, "alice", "Encoding Test", None)
            .await
            .expect("create");

        let payload = ProposalReviewedPayload {
            proposal_id: Id128::from_u128(pid.as_u128()),
            reviewer: "bob".to_string(),
            decision,
            comment: None,
        };
        worker
            .on_proposal_reviewed(&tok, &payload)
            .await
            .expect("on_proposal_reviewed must succeed");

        let sql = rt.sql();
        let mut reader = sql.reader().await.expect("reader");
        let row = reader
            .query_row(SqlStatement {
                sql: "SELECT last_decision FROM proposals_open WHERE proposal_id = ?1".to_string(),
                params: vec![SqlValue::Text(pid.to_string())],
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

    let (cas1, _eid1) = worker
        .withdrawn_and_emit(&tok, pid, make_event())
        .await
        .expect("first withdrawn_and_emit must not error");
    assert!(cas1, "first withdrawn_and_emit must return cas_hit=true");

    let (cas2, _eid2) = worker
        .withdrawn_and_emit(&tok, pid, make_event())
        .await
        .expect("second withdrawn_and_emit must not error");
    assert!(
        !cas2,
        "H1-R3: second withdrawn_and_emit must return cas_hit=false"
    );

    let sql = rt.sql();
    let mut reader = sql.reader().await.expect("reader");
    let count_row = reader
        .query_row(SqlStatement {
            sql: "SELECT COUNT(*) as cnt FROM events WHERE kind='proposal_withdrawn'".to_string(),
            params: vec![],
            label: Some("test.withdrawn_event_count".into()),
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
        "H1-R3: exactly ONE ProposalWithdrawn event must exist; got {count}."
    );
}

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
        assert_eq!(total, 2, "caller A must write 2 rows; got {total}");
    }

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
        assert_eq!(total, 0, "caller B must write 0 rows; got {total}");
    }

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
        "R4: exactly ONE ProposalWithdrawn event must exist; got {count}."
    );
}

#[tokio::test]
async fn on_proposal_applied_cas_fails_when_already_withdrawn() {
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

    let applied = worker
        .on_proposal_applied(&tok, pid)
        .await
        .expect("on_proposal_applied must not error");
    assert!(
        !applied,
        "H1: on_proposal_applied CAS must return false when status='withdrawn'"
    );

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
