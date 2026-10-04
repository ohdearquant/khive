//! Every table a built-in pack creates is classified by the namespace move.
//!
//! `khive-db` cannot see a pack, so a table a pack adds can carry a `namespace`
//! column that the move has no rule for. A source holding rows in such a table
//! then refuses to move, and nothing fails until someone tries. This test boots
//! the full built-in pack set into one store, runs the namespace census over it,
//! and names every table the census finds that the move neither classifies nor
//! excludes.
//!
//! It lives in `khive-mcp` because that crate links every built-in pack and boots
//! them together, which no lower crate can. A pack that is not in
//! `RuntimeConfig::built_in_packs()` is selected explicitly at run time and is
//! outside this population.

use khive_db::namespace_census::{census, TABLES_EXCLUDED_FROM_MOVE};
use khive_db::namespace_move::disposition;
use khive_mcp::server::KhiveMcpServer;
use khive_runtime::{KhiveRuntime, RuntimeConfig};

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn every_built_in_pack_table_has_a_move_disposition_or_an_exclusion() {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: RuntimeConfig::built_in_packs(),
        brain_profile: None,
        ..RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime");
    let _server = KhiveMcpServer::new(runtime.clone()).expect("every built-in pack boots");

    let writer = runtime.backend().pool().writer().expect("writer");
    let inventory = census(writer.conn()).expect("census");

    // The census must contain the tables this guard exists for, or an empty
    // result below would mean the boot applied no pack schema at all.
    for name in [
        "exec_runs",
        "exec_events",
        "git_receipts",
        "tool_policy",
        "tool_grants",
        "gtd_lifecycle_audit",
        "knowledge_eval_runs",
    ] {
        assert!(
            inventory.tables.iter().any(|table| table.name == name),
            "{name} is not in the census, so the boot did not apply its pack's schema"
        );
    }

    let unclassified: Vec<&str> = inventory
        .tables
        .iter()
        .filter(|table| disposition(table).is_none())
        .filter(|table| !TABLES_EXCLUDED_FROM_MOVE.contains(&table.name.as_str()))
        .map(|table| table.name.as_str())
        .collect();
    assert!(
        unclassified.is_empty(),
        "tables the namespace move neither classifies nor excludes: {unclassified:?}"
    );
}
