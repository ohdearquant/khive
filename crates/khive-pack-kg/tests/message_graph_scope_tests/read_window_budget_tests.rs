/// Reads the observer's `args` and counts the edge-list statements SQLite
/// started, in the cursor form or the offset form.
async fn edge_list_reads(fixture: &Fixture, args: Value, cursor: bool) -> (usize, Value) {
    let observation = fixture
        .main
        .backend()
        .pool()
        .observe_test_statement_starts(100_000)
        .expect("statement observation");
    let page = read(&fixture.registry, "observer", "list", args).await;
    let started = observation
        .started_statements()
        .expect("complete statement observation");
    drop(observation);
    let reads = started
        .iter()
        .filter(|statement| {
            if cursor {
                statement
                    .sql
                    .contains("FROM graph_edges_seq CROSS JOIN graph_edges")
            } else {
                statement.sql.starts_with("SELECT namespace, id, source_id")
                    && statement.sql.contains(" OFFSET ?")
            }
        })
        .count();
    (reads, page)
}

/// The most reads of the store one scan may start:
/// `floor(log2(1 + T / w)) + floor(T / C) + 1` for `T` edges passed over, a
/// window `w` that first holds a withheld edge and `C = EDGE_LIST_MAX_LIMIT`.
fn window_formula(passed_over: u32, first_window: u32) -> usize {
    let doublings = (1 + passed_over / first_window).ilog2();
    let at_the_cap = passed_over / KhiveRuntime::EDGE_LIST_MAX_LIMIT;
    (doublings + at_the_cap + 1) as usize
}

async fn assert_a_long_withheld_run_stays_within_the_window_formula(cursor: bool) {
    let f = fixture(Topology::Shared).await;
    let messages = [f.outbound.id, f.inbound.id];
    let runs = [Run::Withheld(2000), Run::Readable(1)];
    let (readable, _) = write_runs(&f.main, &messages, &runs).await;
    // The fixture's own four edges come first, and the one readable edge last.
    let passed_over = f.edges.len() as u32 + 2000 + 1;

    // A filter nothing matches takes one pass, which counts the statements a pass starts.
    let mut args = json!({"kind": "edge", "limit": 1, "relations": ["supports"]});
    if cursor {
        args["after"] = json!("");
    }
    let (per_pass, _) = edge_list_reads(&f, args, cursor).await;
    assert!(per_pass >= 1, "no edge-list statement was recognised");

    let mut args = json!({"kind": "edge", "limit": 1});
    if cursor {
        args["after"] = json!("");
    }
    let (reads, page) = edge_list_reads(&f, args, cursor).await;
    assert_eq!(edge_ids(&page), [readable[0]], "{page}");
    let allowed = per_pass * window_formula(passed_over, 2);
    assert!(
        reads > per_pass,
        "the run is crossed in several passes: {reads} statements"
    );
    assert!(
        reads <= allowed,
        "{reads} edge-list statements for 2000 withheld edges; the formula allows {allowed}"
    );
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn list_edge_cursor_store_reads_stay_within_the_window_formula() {
    assert_a_long_withheld_run_stays_within_the_window_formula(true).await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn list_edge_offset_store_reads_stay_within_the_window_formula() {
    assert_a_long_withheld_run_stays_within_the_window_formula(false).await;
}
