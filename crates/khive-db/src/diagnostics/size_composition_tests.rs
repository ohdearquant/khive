use std::path::PathBuf;
use std::sync::{mpsc, Arc};
use std::time::Duration;

use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};

use super::super::{checkpoint_probe, inspect_pool, inspect_pool_interruptibly, PoolInspection};
use super::test_support::{self, Hooks};
use super::{
    collect, database_page_accounting, Budget, Connection, DatabasePageAccounting, BUDGET,
};
use crate::pool::{ConnectionPool, PoolConfig};

fn fixture() -> (tempfile::TempDir, Arc<ConnectionPool>, PathBuf) {
    let directory = tempfile::tempdir().expect("temporary database");
    let path = directory.path().join("composition.db");
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(path.clone()),
        ..PoolConfig::for_test()
    })
    .expect("private pool");
    pool.try_writer()
        .expect("fixture writer")
        .conn()
        .execute_batch(
            "CREATE TABLE ballast(body BLOB); \
             INSERT INTO ballast VALUES (zeroblob(4096)); \
             CREATE TABLE entities(id TEXT PRIMARY KEY, deleted_at INTEGER, merged_into TEXT); \
             CREATE TABLE graph_edges(namespace TEXT, id TEXT); \
             CREATE TABLE graph_edges_seq(edge_id TEXT); \
             CREATE VIRTUAL TABLE fts_entities USING fts5(body); \
             CREATE VIRTUAL TABLE fts_notes USING fts5(body);",
        )
        .expect("diagnostic fixture");
    (directory, Arc::new(pool), path)
}

async fn inspect(
    pool: Arc<ConnectionPool>,
    interruptible: bool,
    hooks: Hooks,
) -> (PoolInspection, usize) {
    if interruptible {
        // Hooks live on the executing blocking thread, not the async caller.
        crate::read_cancellation::run_interruptible_read(
            khive_storage::StorageCapability::Sql,
            "diagnostics.composition.test",
            move |scope| {
                let _hooks = test_support::install(hooks);
                let report = inspect_pool_interruptibly(&pool, scope, true)?;
                Ok((report, test_support::entries()))
            },
        )
        .await
        .expect("joined interruptible inspection")
    } else {
        let _hooks = test_support::install(hooks);
        (inspect_pool(&pool), test_support::entries())
    }
}

fn assert_core_sections(report: &PoolInspection) {
    assert!(report.checkpoint_probe.is_some());
    assert!(report.graph_edge_integrity.is_some());
    assert!(report.fts_segments.is_some());
}

#[tokio::test]
async fn size_composition_refuses_over_budget_before_dbstat_entry() {
    let (_directory, pool, _path) = fixture();
    for interruptible in [false, true] {
        let observation = pool
            .observe_test_statement_starts(256)
            .expect("statement spy");
        let (report, entries) = inspect(
            Arc::clone(&pool),
            interruptible,
            Hooks {
                budget: Some(Budget { pages: 0, bytes: 0 }),
                ..Hooks::default()
            },
        )
        .await;
        assert_eq!(entries, 0, "dbstat must not even be entered");
        assert!(!observation
            .started_statements()
            .expect("complete statement observations")
            .iter()
            .any(|statement| statement.sql.contains("FROM dbstat")));
        assert_core_sections(&report);
        let pages = report.page_accounting.expect("cheap accounting retained");
        assert!(report.page_accounting_error.is_none());
        assert!(report.size_composition.is_none());
        let refusal = report.size_composition_error.expect("explicit refusal");
        for field in [
            format!("page_count={}", pages.page_count),
            format!("page_size={}", pages.page_size_bytes),
            format!("bytes={}", pages.page_count * pages.page_size_bytes),
            "max_pages=0".to_owned(),
            "max_bytes=0".to_owned(),
        ] {
            assert!(refusal.contains(&field), "{refusal}");
        }
    }
}

#[test]
fn size_composition_admission_requires_both_limits() {
    assert_eq!((BUDGET.pages, BUDGET.bytes), (8192, 33_554_432));
    for (count, size, free, admitted) in [
        (8192, 4096, 0, true),
        (8193, 512, 0, false), // Page limit alone.
        (512, 65_536, 0, true),
        (513, 65_536, 0, false), // Byte limit alone.
        (8191, 4096, 0, true),
        (0, 4096, 0, true),
        (1, 0, 0, false),
        (1, 1000, 0, false),
        (1, 131_072, 0, false),
        (1, 4096, 2, false),
    ] {
        assert_eq!(
            BUDGET.admits(DatabasePageAccounting {
                page_count: count,
                page_size_bytes: size,
                freelist_pages: free,
            }),
            admitted,
            "count={count}, size={size}, free={free}"
        );
    }
    let overflow = DatabasePageAccounting {
        page_count: 1_u64 << 63,
        page_size_bytes: 512,
        freelist_pages: 0,
    };
    assert!(!Budget {
        pages: u64::MAX,
        bytes: u64::MAX,
    }
    .admits(overflow));
    assert!(BUDGET.refusal(overflow).contains("bytes=overflow"));
}

#[tokio::test]
async fn size_composition_metadata_failure_is_explicit_and_fail_closed() {
    let (_directory, pool, _path) = fixture();
    for interruptible in [false, true] {
        let (report, entries) = inspect(
            Arc::clone(&pool),
            interruptible,
            Hooks {
                metadata_error: true,
                ..Hooks::default()
            },
        )
        .await;
        assert_eq!(entries, 0);
        assert_core_sections(&report);
        assert!(report.page_accounting.is_none());
        assert!(report
            .page_accounting_error
            .expect("named metadata error")
            .contains("injected accounting failure"));
        assert!(report.size_composition.is_none());
        assert!(report
            .size_composition_error
            .expect("explicit composition omission")
            .contains("requires valid page accounting"));
    }
}

#[test]
fn size_composition_keeps_admission_and_walk_in_one_snapshot() {
    let (_directory, pool, path) = fixture();
    let conn = pool
        .open_standalone_reader(
            crate::pool::StandaloneReaderPurpose::DiagnosticsIndependentSnapshot,
        )
        .expect("reader");
    let before = collect(&conn, true);
    let pages = before.page_accounting.expect("initial accounting");
    let exact_before = before.size_composition.expect("initial exact composition");
    let budget = Budget {
        pages: pages.page_count,
        bytes: pages.page_count * pages.page_size_bytes,
    };
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let _hooks = test_support::install(Hooks {
            budget: Some(budget),
            after_accounting: Some(Box::new(move |conn, observed| {
                assert!(!conn.is_autocommit());
                ready_tx.send(observed).expect("snapshot witness");
                resume_rx
                    .recv_timeout(Duration::from_secs(5))
                    .expect("growth committed before walk resumes");
            })),
            ..Hooks::default()
        });
        let report = collect(&conn, true);
        (report, conn.is_autocommit(), test_support::entries())
    });
    assert_eq!(
        ready_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("accounting read inside active snapshot"),
        pages
    );
    let writer = Connection::open(path).expect("independent writer");
    writer
        .execute("INSERT INTO ballast VALUES (zeroblob(65536))", [])
        .expect("grow and commit while reader is held");
    let grown = database_page_accounting(&writer).expect("grown accounting");
    assert!(grown.page_count > pages.page_count);
    let pinned = checkpoint_probe(&writer).expect("held-reader checkpoint witness");
    assert!(pinned.log_frames > pinned.checkpointed_frames, "{pinned:?}");
    resume_tx.send(()).expect("resume real worker");
    let (report, autocommit, entries) = worker.join().expect("real worker joined");
    assert!(autocommit, "read transaction released before return");
    assert_eq!(entries, 1);
    assert_eq!(report.page_accounting, Some(pages));
    assert_eq!(report.size_composition, Some(exact_before));
    let released = checkpoint_probe(&writer).expect("released-reader checkpoint witness");
    assert_eq!(released.log_frames, released.checkpointed_frames);
    let _hooks = test_support::install(Hooks {
        budget: Some(budget),
        ..Hooks::default()
    });
    let next = collect(&writer, true);
    assert_eq!(next.page_accounting, Some(grown));
    assert!(next.size_composition.is_none());
    assert!(next
        .size_composition_error
        .expect("fresh refusal")
        .contains("admission refused"));
    assert_eq!(test_support::entries(), 0);
    assert!(writer.is_autocommit());
}

#[test]
fn size_composition_releases_snapshot_on_every_outcome() {
    let (_directory, pool, _path) = fixture();
    let conn = pool
        .open_standalone_reader(
            crate::pool::StandaloneReaderPurpose::DiagnosticsIndependentSnapshot,
        )
        .expect("reader");
    for outcome in ["success", "skip", "metadata", "dbstat", "not_requested"] {
        let _hooks = test_support::install(Hooks {
            budget: (outcome == "skip").then_some(Budget { pages: 0, bytes: 0 }),
            metadata_error: outcome == "metadata",
            ..Hooks::default()
        });
        if outcome == "dbstat" {
            conn.authorizer(Some(|context: AuthContext<'_>| match context.action {
                AuthAction::Read {
                    table_name: "dbstat",
                    ..
                } => Authorization::Deny,
                _ => Authorization::Allow,
            }))
            .expect("deny dbstat preparation");
        }
        let report = collect(&conn, outcome != "not_requested");
        conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
            .expect("remove query fault");
        assert!(conn.is_autocommit(), "{outcome} leaked its transaction");
        assert_eq!(report.size_composition.is_some(), outcome == "success");
        if outcome == "dbstat" {
            assert!(report
                .size_composition_error
                .as_deref()
                .expect("SQL failure")
                .contains("query failed"));
            assert_eq!(test_support::entries(), 1);
        } else if outcome != "success" {
            assert_eq!(test_support::entries(), 0);
        }
        if outcome == "not_requested" {
            assert_eq!(
                report.size_composition_error.as_deref(),
                Some(super::SIZE_COMPOSITION_NOT_REQUESTED)
            );
        }
    }
}

#[test]
fn size_composition_cleanup_failure_cannot_report_success() {
    let (_directory, pool, _path) = fixture();
    let conn = pool
        .open_standalone_reader(
            crate::pool::StandaloneReaderPurpose::DiagnosticsIndependentSnapshot,
        )
        .expect("reader");
    conn.authorizer(Some(|context: AuthContext<'_>| match context.action {
        AuthAction::Transaction {
            operation: TransactionOperation::Rollback,
        } => Authorization::Deny,
        _ => Authorization::Allow,
    }))
    .expect("deny rollback");
    let report = collect(&conn, true);
    assert!(report.size_composition.is_none());
    assert!(report
        .size_composition_error
        .expect("cleanup failure")
        .contains("cleanup failed"));
    conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .expect("remove cleanup fault");
    conn.execute_batch("ROLLBACK")
        .expect("release deliberately refused cleanup");
    assert!(conn.is_autocommit());
}

#[test]
fn size_composition_budget_is_explicit_in_serialized_report() {
    let (_directory, pool, _path) = fixture();
    let _hooks = test_support::install(Hooks {
        budget: Some(Budget { pages: 0, bytes: 0 }),
        ..Hooks::default()
    });
    let report = super::super::collect(
        &pool,
        super::super::BuildIdentity::from_env("composition-budget-test", None),
        Duration::from_secs(30),
    );
    let wire = serde_json::to_value(report).expect("serialized diagnostic report");
    assert!(wire["size_composition"].is_null());
    assert!(wire["size_composition_error"]
        .as_str()
        .expect("wire refusal")
        .contains("admission refused"));
    assert!(wire["page_accounting"]["page_count"].as_u64().is_some());
    assert!(wire["graph_edge_integrity"].is_object());
    assert!(wire["fts_segments"].is_object());
    assert_eq!(test_support::entries(), 0);
}
