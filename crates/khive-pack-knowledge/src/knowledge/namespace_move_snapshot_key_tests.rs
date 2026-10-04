//! A namespace move deletes the source's Vamana snapshots by their composite key.
//!
//! `khive-db` cannot name [`snapshot_key`], so the move restates its `::`
//! separator. This is the test in a crate that sees both: it fails if either
//! side changes the separator without the other.

use crate::knowledge::vamana::snapshot_key;
use khive_db::migrations::run_migrations;
use khive_db::namespace_move::{move_namespace, MoveRequest, MoveRoute, SubjectClass};
use rusqlite::Connection;

#[test]
fn the_move_deletes_the_key_the_vamana_module_writes_and_only_that() {
    let mut conn = Connection::open_in_memory().expect("open");
    run_migrations(&mut conn).expect("migrate");
    conn.execute_batch(
        "CREATE TABLE retrieval_snapshots (\
         namespace TEXT NOT NULL, index_type TEXT NOT NULL, snapshot BLOB NOT NULL, \
         created_at INTEGER NOT NULL, PRIMARY KEY (namespace, index_type))",
    )
    .expect("snapshot table");
    for namespace in ["local", "localized"] {
        conn.execute(
            "INSERT INTO retrieval_snapshots (namespace, index_type, snapshot, created_at) \
             VALUES (?1, 'vamana', x'00', 1)",
            [snapshot_key(namespace, "model-a")],
        )
        .expect("seed snapshot");
    }

    let request = MoveRequest::new(
        "local",
        vec![MoveRoute {
            class: SubjectClass::Atom,
            target: "moved-to".to_string(),
        }],
    );
    move_namespace(&conn, &request).expect("a move with nothing routed here succeeds");

    let mut stmt = conn
        .prepare("SELECT namespace FROM retrieval_snapshots ORDER BY namespace")
        .expect("prepare");
    let kept = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query")
        .collect::<rusqlite::Result<Vec<String>>>()
        .expect("rows");
    assert_eq!(kept, [snapshot_key("localized", "model-a")]);
}
