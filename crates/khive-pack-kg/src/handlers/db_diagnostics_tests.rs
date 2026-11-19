use serde_json::json;

use std::time::Duration;

use super::{
    annotate_graph_edge_integrity, assemble_backend_diagnostics, inspection_window,
    BackendDiagnosticResult, DEADLINE_INTERRUPTED, DEADLINE_NOT_REACHED,
};

#[test]
fn inspections_leave_a_reserve_before_the_request_deadline() {
    // The default 30 s deadline keeps the 2 s cap; shorter deadlines keep a tenth.
    assert_eq!(
        inspection_window(Duration::from_secs(30)),
        Duration::from_secs(28)
    );
    assert_eq!(
        inspection_window(Duration::from_secs(1)),
        Duration::from_millis(900)
    );
    assert_eq!(inspection_window(Duration::ZERO), Duration::ZERO);
}

#[test]
fn deadline_markers_share_the_documented_prefix() {
    for marker in [DEADLINE_INTERRUPTED, DEADLINE_NOT_REACHED] {
        assert!(
            marker.starts_with("not inspected: request deadline"),
            "{marker}"
        );
    }
    assert!(crate::handler_defs::KG_HANDLERS
        .iter()
        .find(|h| h.name == "db_diagnostics")
        .expect("db_diagnostics is declared")
        .description
        .contains("not inspected: request deadline"));
}

#[test]
fn one_backend_diagnostics_error_keeps_other_entries() {
    let result = assemble_backend_diagnostics(
        vec![
        BackendDiagnosticResult {
            backend_names: vec!["main".into()],
            path: Some("/main.db".into()),
            result: Ok(json!({"db_path": "/main.db", "reader_contention": {"reader_acquisitions": 1}})),
        },
        BackendDiagnosticResult {
            backend_names: vec!["broken".into()],
            path: Some("/broken.db".into()),
            result: Err("inspection failed".into()),
        },
        BackendDiagnosticResult {
            backend_names: vec!["healthy".into()],
            path: Some("/healthy.db".into()),
            result: Ok(json!({"db_path": "/healthy.db", "reader_contention": {"reader_acquisitions": 3}})),
        },
    ],
        true,
    )
    .expect("one failed file must not fail the verb");
    assert_eq!(result["db_path"], "/main.db");
    assert_eq!(result["complete"], true);
    assert_eq!(result["databases"].as_array().unwrap().len(), 3);
    assert_eq!(result["databases"][1]["error"], "inspection failed");
    assert!(result["databases"][1]["diagnostics"].is_null());
    assert_eq!(
        result["databases"][2]["diagnostics"]["db_path"],
        "/healthy.db"
    );
}

#[test]
fn graph_edge_integrity_explains_retained_delete_history() {
    let mut report = json!({
        "graph_edge_integrity": {
            "graph_edges_rows": 3,
            "graph_edges_seq_rows": 5,
        }
    });

    annotate_graph_edge_integrity(&mut report);

    let integrity = &report["graph_edge_integrity"];
    assert_eq!(
        integrity["graph_edges_rows_scope"],
        json!({"namespaces": "all", "rows": "live_and_soft_deleted"})
    );
    assert_eq!(
        integrity["graph_edges_seq_rows_scope"],
        json!({
            "namespaces": "all",
            "rows": "inserted_ids_retained_after_hard_delete",
        })
    );
    assert_eq!(integrity["graph_edges_seq_minus_graph_edges"], 2);
    assert_eq!(
        integrity["graph_edges_seq_relationship"],
        "ledger_ahead_consistent_with_hard_deletes"
    );
}

#[test]
fn graph_edge_integrity_marks_a_ledger_deficit_unexpected() {
    let mut report = json!({
        "graph_edge_integrity": {
            "graph_edges_rows": 3,
            "graph_edges_seq_rows": 2,
        }
    });

    annotate_graph_edge_integrity(&mut report);

    let integrity = &report["graph_edge_integrity"];
    assert_eq!(integrity["graph_edges_seq_minus_graph_edges"], -1);
    assert_eq!(
        integrity["graph_edges_seq_relationship"],
        "ledger_behind_unexpected"
    );
}

#[test]
fn graph_edge_integrity_classifies_pre_v14_duplicate_state_as_legacy() {
    // Mirrors the khive-db regression fixture: two graph_edges rows share
    // one ledger row and the report flags the pre-V14 duplicate state.
    let mut report = json!({
        "graph_edge_integrity": {
            "graph_edges_rows": 2,
            "graph_edges_seq_rows": 1,
            "pre_v14_duplicate_edge_state_detected": true,
        }
    });

    annotate_graph_edge_integrity(&mut report);

    let integrity = &report["graph_edge_integrity"];
    assert_eq!(integrity["graph_edges_seq_minus_graph_edges"], -1);
    assert_eq!(
        integrity["graph_edges_seq_relationship"],
        "ledger_behind_pre_v14_duplicate_edge_state"
    );
}
