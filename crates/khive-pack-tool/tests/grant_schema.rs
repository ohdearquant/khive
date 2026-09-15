use khive_pack_kg::KgPack;
use khive_pack_tool::ToolPack;
use khive_runtime::{KhiveRuntime, VerbRegistryBuilder};
use khive_storage::{SqlStatement, SqlValue};
use serde_json::{json, Value};

const LEGACY: &str = "CREATE TABLE tool_grants (
    id TEXT PRIMARY KEY, namespace TEXT NOT NULL, actor TEXT NOT NULL,
    tool TEXT NOT NULL, scope TEXT, reason TEXT, status TEXT NOT NULL,
    requested_at INTEGER NOT NULL, decided_at INTEGER, decided_by TEXT,
    expires_at INTEGER, decision_note TEXT
);
CREATE INDEX idx_tool_grants_lookup ON tool_grants(namespace, actor, tool, status);";

const ORIGINAL_ROWS: &str = "SELECT id, namespace, actor, tool, scope, reason, status,
    requested_at, decided_at, decided_by, expires_at, decision_note
    FROM tool_grants ORDER BY id";

fn install(rt: &KhiveRuntime) -> Result<(), khive_runtime::PackSchemaCollisionError> {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(rt.clone()));
    builder.register(ToolPack::new(rt.clone()));
    builder
        .build()
        .unwrap()
        .apply_schema_plans_with_map(&Default::default(), rt.backend())
}

fn legacy_runtime() -> KhiveRuntime {
    let rt = KhiveRuntime::memory().unwrap();
    rt.backend().apply_pack_ddl_statements(&[LEGACY]).unwrap();
    rt
}

async fn rows(rt: &KhiveRuntime, sql: &str) -> Value {
    let rows = rt
        .sql()
        .reader()
        .await
        .unwrap()
        .query_all(SqlStatement {
            sql: sql.into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
    json!(rows
        .into_iter()
        .map(|row| row
            .columns
            .into_iter()
            .map(|column| {
                match column.value {
                    SqlValue::Null => Value::Null,
                    SqlValue::Text(value) => json!(value),
                    SqlValue::Integer(value) => json!(value),
                    other => panic!("unexpected schema fixture value: {other:?}"),
                }
            })
            .collect::<Vec<_>>())
        .collect::<Vec<_>>())
}

async fn write(rt: &KhiveRuntime, sql: &str, params: Vec<SqlValue>) {
    rt.sql()
        .writer()
        .await
        .unwrap()
        .execute(SqlStatement {
            sql: sql.into(),
            params,
            label: None,
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn tool_schema_is_absent_until_the_pack_is_loaded() {
    let rt = KhiveRuntime::memory().unwrap();
    let query = "SELECT name FROM sqlite_master WHERE name IN ('tool_policy', 'tool_grants', 'tool_grants_invalidate_on_registry_insert') ORDER BY name";
    assert_eq!(rows(&rt, query).await, json!([]));
    install(&rt).unwrap();
    assert_eq!(
        rows(&rt, query).await,
        json!([
            ["tool_grants"],
            ["tool_grants_invalidate_on_registry_insert"],
            ["tool_policy"]
        ])
    );
    let columns = rows(&rt, "SELECT name, type, \"notnull\", dflt_value, pk, hidden FROM pragma_table_xinfo('tool_grants') WHERE cid >= 12 ORDER BY cid").await;
    assert_eq!(
        columns,
        json!([
            ["registry_id", "TEXT", 0, null, 0, 0],
            ["definition_digest", "TEXT", 0, null, 0, 0],
            ["invalidated_by_registry_id", "TEXT", 0, null, 0, 0],
            ["invalidated_at", "INTEGER", 0, null, 0, 0],
        ])
    );
}

#[tokio::test]
async fn tool_schema_preserves_legacy_decisions_and_current_pins_on_reapply() {
    let rt = legacy_runtime();
    for (i, status) in ["requested", "granted", "denied", "revoked"]
        .iter()
        .enumerate()
    {
        write(&rt, "INSERT INTO tool_grants (id, namespace, actor, tool, scope, reason, status, requested_at, decided_at, decided_by, expires_at, decision_note) VALUES (?1, 'local', 'requester', 'tool:test', 'one-run', 'reason', ?2, 100, 200, 'reviewer', 300, 'decision')", vec![SqlValue::Text(format!("grant-{i}")), SqlValue::Text((*status).into())]).await;
    }
    let before = rows(&rt, ORIGINAL_ROWS).await;
    install(&rt).unwrap();
    assert_eq!(rows(&rt, ORIGINAL_ROWS).await, before);
    assert_eq!(rows(&rt, "SELECT registry_id, definition_digest, invalidated_by_registry_id, invalidated_at FROM tool_grants ORDER BY id").await, json!([[null, null, null, null], [null, null, null, null], [null, null, null, null], [null, null, null, null]]));
    write(&rt, "UPDATE tool_grants SET registry_id='approved-id', definition_digest='approved-digest' WHERE status='granted'", vec![]).await;
    let pinned = rows(&rt, "SELECT * FROM tool_grants ORDER BY id").await;
    let schema = rows(&rt, "PRAGMA schema_version").await;
    install(&rt).unwrap();
    assert_eq!(
        rows(&rt, "SELECT * FROM tool_grants ORDER BY id").await,
        pinned
    );
    assert_eq!(rows(&rt, "PRAGMA schema_version").await, schema);
}

#[tokio::test]
async fn tool_schema_completes_compatible_partial_additions_without_rewriting_values() {
    let rt = legacy_runtime();
    rt.backend()
        .apply_pack_ddl_statements(&["ALTER TABLE tool_grants ADD COLUMN registry_id TEXT"])
        .unwrap();
    write(&rt, "INSERT INTO tool_grants (id, namespace, actor, tool, status, requested_at, registry_id) VALUES ('partial', 'local', 'requester', 'tool:test', 'granted', 100, 'existing-pin')", vec![]).await;
    install(&rt).unwrap();
    assert_eq!(rows(&rt, "SELECT registry_id, definition_digest, invalidated_by_registry_id, invalidated_at FROM tool_grants").await, json!([["existing-pin", null, null, null]]));
}

#[tokio::test]
async fn tool_schema_rejects_incompatible_additions_without_changes() {
    for incompatible in [
        "ALTER TABLE tool_grants ADD COLUMN registry_id INTEGER",
        "ALTER TABLE tool_grants ADD COLUMN definition_digest TEXT DEFAULT 'unapproved'",
        "ALTER TABLE tool_grants ADD COLUMN registry_id TEXT NOT NULL",
        "ALTER TABLE tool_grants ADD COLUMN invalidated_at TEXT",
        "ALTER TABLE tool_grants ADD COLUMN definition_digest TEXT GENERATED ALWAYS AS (tool) VIRTUAL",
    ] {
        let rt = legacy_runtime();
        rt.backend().apply_pack_ddl_statements(&[incompatible]).unwrap();
        let before = rows(&rt, "SELECT * FROM pragma_table_xinfo('tool_grants')").await;
        let error = install(&rt).unwrap_err();
        assert!(error.to_string().contains("tool_grants"), "{error}");
        assert_eq!(rows(&rt, "SELECT * FROM pragma_table_xinfo('tool_grants')").await, before);
        assert_eq!(rows(&rt, "SELECT name FROM sqlite_master WHERE name IN ('tool_policy', 'tool_grants_invalidate_on_registry_insert')").await, json!([]));
    }
}

#[tokio::test]
async fn tool_schema_backfills_markers_without_approving_legacy_rows() {
    let rt = legacy_runtime();
    for (id, tool, status, namespace) in [
        ("all", "*", "granted", "local"),
        ("case-prefix", "TOOL:*", "granted", "local"),
        ("exact", "Tool:Exact", "granted", "local"),
        ("foreign", "tool:exact", "granted", "elsewhere"),
        ("future", "new:*", "granted", "local"),
        ("prefix", "tool:*", "granted", "local"),
        ("requested", "tool:exact", "requested", "local"),
    ] {
        write(&rt, "INSERT INTO tool_grants (id, namespace, actor, tool, status, requested_at) VALUES (?1, ?2, 'requester', ?3, ?4, 1)", vec![SqlValue::Text(id.into()), SqlValue::Text(namespace.into()), SqlValue::Text(tool.into()), SqlValue::Text(status.into())]).await;
    }
    let before = rows(&rt, ORIGINAL_ROWS).await;
    for (id, name, namespace, created_at, deleted_at, kind, tags) in [
        (
            "b",
            "tool:b",
            "local",
            100,
            None,
            "project",
            "[\"tool-registry\"]",
        ),
        (
            "a",
            "tool:a",
            "local",
            100,
            None,
            "project",
            "[\"TOOL-REGISTRY\"]",
        ),
        (
            "exact-id",
            "tool:exact",
            "local",
            200,
            None,
            "project",
            "[\"tool-registry\"]",
        ),
        (
            "deleted",
            "tool:deleted",
            "local",
            1,
            Some(10),
            "project",
            "[\"tool-registry\"]",
        ),
        (
            "untagged",
            "tool:untagged",
            "local",
            2,
            None,
            "project",
            "[]",
        ),
        (
            "non-project",
            "tool:concept",
            "local",
            3,
            None,
            "concept",
            "[\"tool-registry\"]",
        ),
        (
            "other-namespace",
            "tool:other",
            "other",
            4,
            None,
            "project",
            "[\"tool-registry\"]",
        ),
    ] {
        write(&rt, "INSERT INTO entities (id, namespace, kind, name, tags, created_at, updated_at, deleted_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7)", vec![SqlValue::Text(id.into()), SqlValue::Text(namespace.into()), SqlValue::Text(kind.into()), SqlValue::Text(name.into()), SqlValue::Text(tags.into()), SqlValue::Integer(created_at), deleted_at.map_or(SqlValue::Null, SqlValue::Integer)]).await;
    }
    install(&rt).unwrap();
    assert_eq!(rows(&rt, ORIGINAL_ROWS).await, before);
    assert_eq!(rows(&rt, "SELECT COUNT(*) FROM tool_grants WHERE registry_id IS NOT NULL OR definition_digest IS NOT NULL").await, json!([[0]]));
    let markers =
        "SELECT id, invalidated_by_registry_id, invalidated_at FROM tool_grants ORDER BY id";
    assert_eq!(
        rows(&rt, markers).await,
        json!([
            ["all", "a", 100],
            ["case-prefix", null, null],
            ["exact", "exact-id", 200],
            ["foreign", null, null],
            ["future", null, null],
            ["prefix", "a", 100],
            ["requested", null, null]
        ])
    );
    write(
        &rt,
        "UPDATE entities SET deleted_at=400 WHERE id='exact-id'",
        vec![],
    )
    .await;
    write(&rt, "INSERT INTO entities (id, namespace, kind, name, tags, created_at, updated_at) VALUES ('future-id', 'local', 'project', 'new:tool', '[\"tool-registry\"]', 500, 500)", vec![]).await;
    write(&rt, "DELETE FROM entities WHERE id='future-id'", vec![]).await;
    let after = rows(&rt, markers).await;
    assert_eq!(after[0], json!(["all", "a", 100]));
    assert_eq!(after[2], json!(["exact", "exact-id", 200]));
    assert_eq!(after[4], json!(["future", "future-id", 500]));
    install(&rt).unwrap();
    assert_eq!(rows(&rt, markers).await, after);
}

#[tokio::test]
async fn tool_schema_rolls_back_additions_if_marker_backfill_fails() {
    let rt = legacy_runtime();
    write(&rt, "INSERT INTO tool_grants (id, namespace, actor, tool, status, requested_at) VALUES ('legacy', 'local', 'requester', '*', 'granted', 1)", vec![]).await;
    write(&rt, "INSERT INTO entities (id, namespace, kind, name, tags, created_at, updated_at) VALUES ('registry', 'local', 'project', 'tool:test', '[\"tool-registry\"]', 100, 100)", vec![]).await;
    rt.backend().apply_pack_ddl_statements(&["CREATE TRIGGER refuse_backfill BEFORE UPDATE ON tool_grants BEGIN SELECT RAISE(ABORT, 'marker backfill unavailable'); END;"]).unwrap();
    let before = rows(&rt, ORIGINAL_ROWS).await;
    let schema = rows(&rt, "SELECT * FROM pragma_table_xinfo('tool_grants')").await;
    let error = install(&rt).unwrap_err();
    assert!(
        error.to_string().contains("marker backfill unavailable"),
        "{error}"
    );
    assert_eq!(rows(&rt, ORIGINAL_ROWS).await, before);
    assert_eq!(
        rows(&rt, "SELECT * FROM pragma_table_xinfo('tool_grants')").await,
        schema
    );
    assert_eq!(rows(&rt, "SELECT name FROM sqlite_master WHERE name IN ('tool_policy', 'tool_grants_invalidate_on_registry_insert')").await, json!([]));
    rt.backend()
        .apply_pack_ddl_statements(&["DROP TRIGGER refuse_backfill"])
        .unwrap();
    install(&rt).unwrap();
    assert_eq!(rows(&rt, "SELECT registry_id, definition_digest, invalidated_by_registry_id, invalidated_at FROM tool_grants").await, json!([[null, null, "registry", 100]]));
}
