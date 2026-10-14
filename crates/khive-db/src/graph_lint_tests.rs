use super::*;
use khive_storage::SqlColumn;

fn row() -> SqlRow {
    SqlRow {
        columns: vec![
            ("id", SqlValue::Text(Uuid::new_v4().to_string())),
            ("namespace", SqlValue::Text("local".into())),
            ("source_id", SqlValue::Text(Uuid::new_v4().to_string())),
            ("target_id", SqlValue::Text(Uuid::new_v4().to_string())),
            ("relation", SqlValue::Text("extends".into())),
            ("weight", SqlValue::Float(0.3)),
            ("created_at", SqlValue::Integer(10)),
            ("updated_at", SqlValue::Integer(20)),
            ("deleted_at", SqlValue::Null),
            ("metadata", SqlValue::Text("{ \"z\": 2, \"a\": 1 }".into())),
            ("target_backend", SqlValue::Null),
        ]
        .into_iter()
        .map(|(name, value)| SqlColumn {
            name: name.into(),
            value,
        })
        .collect(),
    }
}
fn replace(row: &mut SqlRow, column: &str, value: SqlValue) {
    row.columns
        .iter_mut()
        .find(|entry| entry.name == column)
        .unwrap()
        .value = value;
}
#[test]
fn graph_decode_refuses_nonfinite_malformed_and_out_of_range_values() {
    let valid = row();
    assert_eq!(decode_edge(&valid).unwrap().weight, 0.3);
    for (column, value) in [
        ("weight", SqlValue::Float(f64::NAN)),
        ("weight", SqlValue::Float(f64::INFINITY)),
        ("created_at", SqlValue::Integer(i64::MAX)),
        ("deleted_at", SqlValue::Integer(i64::MIN)),
        ("metadata", SqlValue::Text("broken".into())),
        ("id", SqlValue::Text("not UUID".into())),
        ("relation", SqlValue::Text("not-relation".into())),
        ("weight", SqlValue::Null),
    ] {
        let mut invalid = valid.clone();
        replace(&mut invalid, column, value);
        assert!(decode_edge(&invalid).is_err(), "{column}");
    }
    let mut missing = valid;
    missing.columns.retain(|column| column.name != "deleted_at");
    assert!(decode_edge(&missing).is_err());
}
#[test]
fn exact_edge_predicate_keeps_raw_metadata_and_rejects_every_changed_field() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(include_str!("../sql/graph-ddl.sql"))
        .unwrap();
    let row = row();
    let statement = edge_witness(&row).unwrap();
    let mut insert=conn.prepare("INSERT INTO graph_edges(id,namespace,source_id,target_id,relation,weight,created_at,updated_at,deleted_at,metadata,target_backend) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)").unwrap();
    crate::sql_bridge::bind_params(&mut insert, &statement.params).unwrap();
    insert.raw_execute().unwrap();
    drop(insert);
    let matches = |query: &SqlStatement| {
        let mut prepared = conn.prepare(&query.sql).unwrap();
        crate::sql_bridge::bind_params(&mut prepared, &query.params).unwrap();
        let found = prepared.raw_query().next().unwrap().is_some();
        found
    };
    assert!(
        matches(&statement),
        "unchanged noncanonical JSON spelling must match"
    );
    for index in 0..statement.params.len() {
        let mut changed = statement.clone();
        changed.params[index] = match &changed.params[index] {
            SqlValue::Integer(value) => SqlValue::Integer(value + 1),
            SqlValue::Float(value) => SqlValue::Float(value + 0.1),
            SqlValue::Text(value) => SqlValue::Text(format!("{value}x")),
            SqlValue::Null => SqlValue::Integer(1),
            _ => unreachable!(),
        };
        assert!(
            !matches(&changed),
            "predicate parameter {index} is required"
        );
    }
}
#[test]
fn entity_revision_predicate_rejects_restore_rename_and_same_time_update() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(include_str!("../sql/entities-ddl.sql"))
        .unwrap();
    // The owning runtime materializes the exact observed revision; timestamps
    // alone cannot distinguish these two writes.
    let entity = Entity::minimal(Uuid::new_v4(), "local", "concept", "name", 11, 11);
    let insert = crate::stores::entity::entity_upsert_statement(&entity);
    let mut prepared = conn.prepare(&insert.sql).unwrap();
    crate::sql_bridge::bind_params(&mut prepared, &insert.params).unwrap();
    prepared.raw_execute().unwrap();
    drop(prepared);
    let guard = entity_witness(&entity);
    let matches = || {
        let mut prepared = conn.prepare(&guard.sql).unwrap();
        crate::sql_bridge::bind_params(&mut prepared, &guard.params).unwrap();
        let found = prepared.raw_query().next().unwrap().is_some();
        found
    };
    assert!(matches());
    conn.execute(
        "UPDATE entities SET version=version+1 WHERE id=?1",
        [entity.id.to_string()],
    )
    .unwrap();
    assert!(!matches());
    conn.execute(
        "UPDATE entities SET version=version+1,deleted_at=8 WHERE id=?1",
        [entity.id.to_string()],
    )
    .unwrap();
    assert!(!matches());
    conn.execute(
        "UPDATE entities SET version=version+1,deleted_at=NULL,name='other' WHERE id=?1",
        [entity.id.to_string()],
    )
    .unwrap();
    assert!(!matches());
}
