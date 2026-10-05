use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use khive_runtime::pack::{PackRegistry, VerbRegistryBuilder};
use khive_runtime::{KhiveRuntime, RuntimeConfig};
use khive_types::{Pack, PackColumnAddition, PackColumnAffinity, PackSchemaPlan};
use kkernel as _;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};

const CORE_TABLES: [&str; 4] = ["notes", "entities", "graph_edges", "events"];
// ADR-017 (2026-09-12) mentions tool invalidation; #4137 tracks this shipped exception.
const TOOL_INVALIDATION_SHA256: &str =
    "c025d184512b437533e641e2e1c4427f585bd23aae23bd9359e76d3720f6b01e";
const TOOL_INVALIDATION_TRIGGER: &str = "tool_grants_invalidate_on_registry_insert";

struct Declared {
    name: &'static str,
    plan: Option<PackSchemaPlan>,
    columns: &'static [PackColumnAddition],
}

fn declared<P: Pack>() -> Declared {
    Declared {
        name: P::NAME,
        plan: P::SCHEMA_PLAN,
        columns: P::SCHEMA_COLUMN_ADDITIONS,
    }
}

fn declarations() -> Vec<Declared> {
    #[allow(unused_mut)]
    let mut rows = vec![
        declared::<khive_pack_blob::BlobPack>(),
        declared::<khive_pack_brain::BrainPack>(),
        declared::<khive_pack_code::CodePack>(),
        declared::<khive_pack_comm::CommPack>(),
        declared::<khive_pack_exec::ExecPack>(),
        declared::<khive_pack_git::GitPack>(),
        declared::<khive_pack_gtd::GtdPack>(),
        declared::<khive_pack_kg::KgPack>(),
        declared::<khive_pack_knowledge::KnowledgePack>(),
        declared::<khive_pack_memory::MemoryPack>(),
        declared::<khive_pack_schedule::SchedulePack>(),
        declared::<khive_pack_session::SessionPack>(),
        declared::<khive_pack_template::TemplatePack>(),
        declared::<khive_pack_tool::ToolPack>(),
        declared::<khive_pack_workspace::WorkspacePack>(),
    ];
    #[cfg(feature = "pack-agent")]
    rows.push(declared::<khive_pack_agent::AgentPack>());
    #[cfg(feature = "pack-telemetry")]
    rows.push(declared::<khive_pack_telemetry::TelemetryPack>());
    #[cfg(feature = "pack-web")]
    rows.push(declared::<khive_pack_web::WebPack>());
    #[cfg(feature = "pack-formal")]
    rows.push(declared::<khive_pack_formal::FormalPack>());
    #[cfg(feature = "pack-moodboard")]
    rows.push(declared::<khive_pack_moodboard::MoodboardPack>());
    rows
}

fn is_core(table: &str) -> bool {
    CORE_TABLES
        .iter()
        .any(|core| core.eq_ignore_ascii_case(table))
}

fn trusted_tool_batch(pack: &str, sql: &str) -> bool {
    pack == "tool"
        && format!(
            "{:x}",
            Sha256::digest(sql.replace("\r\n", "\n").trim().as_bytes())
        ) == TOOL_INVALIDATION_SHA256
}

fn refused_action(action: AuthAction<'_>, tool_batch: bool) -> Option<String> {
    if tool_batch {
        match action {
            AuthAction::CreateTrigger {
                trigger_name,
                table_name,
            } if trigger_name == TOOL_INVALIDATION_TRIGGER && table_name == "entities" => {
                return None
            }
            AuthAction::Read {
                table_name: "entities",
                ..
            } => return None,
            _ => {}
        }
    }
    if matches!(
        action,
        AuthAction::CreateTempTrigger { .. } | AuthAction::CreateTempView { .. }
    ) {
        return Some(format!(
            "temporary trigger/view plans are not covered: {action:?}"
        ));
    }
    let table = match action {
        AuthAction::CreateIndex { table_name, .. }
        | AuthAction::CreateTempIndex { table_name, .. }
        | AuthAction::DropIndex { table_name, .. }
        | AuthAction::DropTempIndex { table_name, .. }
        | AuthAction::CreateTable { table_name }
        | AuthAction::CreateTempTable { table_name }
        | AuthAction::DropTable { table_name }
        | AuthAction::DropTempTable { table_name }
        | AuthAction::AlterTable { table_name, .. }
        | AuthAction::CreateTrigger { table_name, .. }
        | AuthAction::CreateTempTrigger { table_name, .. }
        | AuthAction::DropTrigger { table_name, .. }
        | AuthAction::DropTempTrigger { table_name, .. }
        | AuthAction::CreateVtable { table_name, .. }
        | AuthAction::DropVtable { table_name, .. }
        | AuthAction::Read { table_name, .. }
        | AuthAction::Insert { table_name }
        | AuthAction::Delete { table_name }
        | AuthAction::Update { table_name, .. }
        | AuthAction::Analyze { table_name } => Some(table_name),
        AuthAction::CreateView { view_name }
        | AuthAction::CreateTempView { view_name }
        | AuthAction::DropView { view_name }
        | AuthAction::DropTempView { view_name } => Some(view_name),
        AuthAction::Attach { .. }
        | AuthAction::Detach { .. }
        | AuthAction::Reindex { .. }
        | AuthAction::Unknown { .. } => return Some(format!("unsupported plan action {action:?}")),
        AuthAction::Pragma { pragma_name, .. }
            if pragma_name.eq_ignore_ascii_case("writable_schema") =>
        {
            return Some(format!("unsupported plan action {action:?}"));
        }
        _ => None,
    };
    table
        .filter(|table| is_core(table))
        .map(|table| format!("core table {table}: {action:?}"))
}

fn quoted(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn scope_connection(migrated: bool) -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    if migrated {
        khive_db::run_migrations(&mut conn).unwrap();
        let indexes: Vec<(String, String)> = conn
            .prepare(
                "SELECT name,tbl_name FROM sqlite_schema WHERE type='index' AND sql IS NOT NULL",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        // Otherwise an existing IF NOT EXISTS index never reaches the authorizer.
        for (name, table) in indexes {
            if is_core(&table) {
                conn.execute_batch(&format!("DROP INDEX {}", quoted(&name)))
                    .unwrap();
            }
        }
    }
    conn
}

fn check_statement(conn: &Connection, pack: &str, sql: &str) -> Result<(), String> {
    inspect_sql(conn, pack, sql, false)
}

fn inspect_sql(conn: &Connection, pack: &str, sql: &str, prepare_only: bool) -> Result<(), String> {
    let tool_batch = trusted_tool_batch(pack, sql);
    let refused = Arc::new(Mutex::new(None));
    let recorded = Arc::clone(&refused);
    conn.authorizer(Some(move |context: AuthContext<'_>| {
        if let Some(reason) = refused_action(context.action, tool_batch) {
            recorded.lock().unwrap().get_or_insert(reason);
            Authorization::Deny
        } else {
            Authorization::Allow
        }
    }))
    .map_err(|e| e.to_string())?;
    let result = if prepare_only {
        conn.prepare(sql).map(drop)
    } else {
        conn.execute_batch(sql)
    };
    conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .map_err(|e| e.to_string())?;
    if let Some(reason) = refused.lock().unwrap().take() {
        let exception = if pack == "tool" {
            "; tracked #4137 exception requires the exact pinned tool trigger/backfill SQL"
        } else {
            ""
        };
        return Err(format!("{pack}: {reason}{exception}"));
    }
    result.map_err(|e| format!("{pack}: SQL did not prepare/execute: {e}"))
}

fn check_column_targets(pack: &str, additions: &[PackColumnAddition]) -> Result<(), String> {
    for addition in additions {
        if is_core(addition.table) {
            return Err(format!(
                "{pack}: nullable column targets core table {}",
                addition.table
            ));
        }
    }
    Ok(())
}

fn check_column_shapes(
    conn: &Connection,
    pack: &str,
    additions: &[PackColumnAddition],
) -> Result<(), String> {
    check_column_targets(pack, additions)?;
    for addition in additions {
        let expected = match addition.affinity {
            PackColumnAffinity::Text => "TEXT",
            PackColumnAffinity::Integer => "INTEGER",
        };
        let shape = conn.query_row(
            "SELECT type,\"notnull\",dflt_value,pk,hidden FROM pragma_table_xinfo(?1,'main') WHERE name=?2 COLLATE NOCASE",
            [addition.table, addition.column],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?))
        ).optional().map_err(|e| e.to_string())?;
        let valid = shape
            .as_ref()
            .is_some_and(|(kind, notnull, default, pk, hidden)| {
                kind.trim().eq_ignore_ascii_case(expected)
                    && *notnull == 0
                    && default.is_none()
                    && *pk == 0
                    && *hidden == 0
            });
        if !valid {
            return Err(format!(
                "{pack}: invalid nullable auxiliary column {}.{}: {shape:?}",
                addition.table, addition.column
            ));
        }
    }
    Ok(())
}

#[derive(PartialEq, Eq)]
struct DeferredBody {
    kind: String,
    name: String,
    table: String,
    sql: String,
}

fn deferred_bodies(conn: &Connection) -> Result<Vec<DeferredBody>, String> {
    conn.prepare("SELECT type,name,tbl_name,sql FROM main.sqlite_schema WHERE type IN ('trigger','view') ORDER BY name")
        .map_err(|e| e.to_string())?
        .query_map([], |r| Ok(DeferredBody { kind: r.get(0)?, name: r.get(1)?, table: r.get(2)?, sql: r.get(3)? }))
        .map_err(|e| e.to_string())?
        .collect::<rusqlite::Result<_>>()
        .map_err(|e| e.to_string())
}

fn check_deferred_bodies(
    conn: &Connection,
    pack: &str,
    before: &[DeferredBody],
    tool_batch: bool,
) -> Result<(), String> {
    let after = deferred_bodies(conn)?;
    let mut trigger_targets = BTreeSet::new();
    for body in after.iter().filter(|body| !before.contains(body)) {
        if body.kind == "view" {
            inspect_sql(
                conn,
                pack,
                &format!("EXPLAIN SELECT * FROM main.{}", quoted(&body.name)),
                true,
            )?;
        } else if is_core(&body.table) {
            if !(tool_batch && body.table == "entities" && body.name == TOOL_INVALIDATION_TRIGGER) {
                return Err(format!(
                    "{pack}: deferred trigger {} targets core table {}",
                    body.name, body.table
                ));
            }
        } else {
            trigger_targets.insert(body.table.as_str());
        }
    }
    for table in trigger_targets {
        let view = after
            .iter()
            .any(|body| body.kind == "view" && body.name == table);
        if view {
            inspect_sql(
                conn,
                pack,
                &format!("EXPLAIN SELECT * FROM main.{}", quoted(table)),
                true,
            )?;
        }
        let columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_xinfo(?1,'main') WHERE hidden=0 ORDER BY cid")
            .map_err(|e| e.to_string())?
            .query_map([table], |r| r.get(0))
            .map_err(|e| e.to_string())?
            .collect::<rusqlite::Result<_>>()
            .map_err(|e| e.to_string())?;
        if columns.is_empty() {
            return Err(format!(
                "{pack}: trigger target {table} has no ordinary writable columns"
            ));
        }
        let assignments = columns
            .iter()
            .map(|column| {
                let name = quoted(column);
                format!("{name}={name}")
            })
            .collect::<Vec<_>>()
            .join(",");
        let target = format!("main.{}", quoted(table));
        let mut prepared = 0;
        for sql in [
            format!("EXPLAIN INSERT INTO {target} DEFAULT VALUES"),
            format!("EXPLAIN DELETE FROM {target}"),
            format!("EXPLAIN UPDATE {target} SET {assignments}"),
        ] {
            match inspect_sql(conn, pack, &sql, true) {
                Ok(()) => prepared += 1,
                // A view can have only one of the three INSTEAD OF events.
                // Accept only SQLite's exact absent-event error, never a
                // refusal or an arbitrary failure from a trigger body.
                Err(error) if view && error == format!("{pack}: SQL did not prepare/execute: cannot modify {table} because it is a view") => {}
                Err(error) => return Err(error),
            }
        }
        if prepared == 0 {
            return Err(format!("{pack}: no trigger action prepared for {table}"));
        }
    }
    Ok(())
}

fn check_plan(
    conn: &Connection,
    pack: &str,
    statements: &[&str],
    columns: &[PackColumnAddition],
    cold: bool,
) -> Result<(), String> {
    check_column_targets(pack, columns)?;
    let before = deferred_bodies(conn)?;
    for (ordinal, statement) in statements.iter().enumerate() {
        if cold && trusted_tool_batch(pack, statement) {
            // This one shipped batch needs the real core schema. Its entire
            // preceding auxiliary prefix is checked on that disposable fixture.
            let core = scope_connection(true);
            for prefix in &statements[..=ordinal] {
                check_statement(&core, pack, prefix)
                    .map_err(|e| format!("statement {ordinal}, shipped tool exception: {e}"))?;
            }
        } else {
            check_statement(conn, pack, statement)
                .map_err(|e| format!("statement {ordinal}: {e}"))?;
        }
    }
    check_deferred_bodies(
        conn,
        pack,
        &before,
        statements.iter().any(|sql| trusted_tool_batch(pack, sql)),
    )?;
    check_column_shapes(conn, pack, columns)
}

#[test]
fn every_repository_pack_schema_stays_auxiliary() {
    let discovered_rows = PackRegistry::discovered_names();
    let discovered: BTreeSet<_> = discovered_rows.iter().copied().collect();
    assert_eq!(
        discovered.len(),
        discovered_rows.len(),
        "duplicate factory names"
    );
    let mut expected: BTreeSet<String> = RuntimeConfig::built_in_packs().into_iter().collect();
    expected.insert("template".into());
    for (name, enabled) in [
        ("agent", cfg!(feature = "pack-agent")),
        ("telemetry", cfg!(feature = "pack-telemetry")),
        ("web", cfg!(feature = "pack-web")),
        ("formal", cfg!(feature = "pack-formal")),
        ("moodboard", cfg!(feature = "pack-moodboard")),
    ] {
        if enabled {
            expected.insert(name.into());
        }
    }
    let expected_count = 15
        + [
            cfg!(feature = "pack-agent"),
            cfg!(feature = "pack-telemetry"),
            cfg!(feature = "pack-web"),
            cfg!(feature = "pack-formal"),
            cfg!(feature = "pack-moodboard"),
        ]
        .into_iter()
        .filter(|enabled| *enabled)
        .count();
    assert_eq!(
        expected.len(),
        expected_count,
        "review the repository pack census"
    );
    assert_eq!(
        discovered
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<BTreeSet<_>>(),
        expected
    );
    let declarations = declarations();
    let declared_names: BTreeSet<_> = declarations.iter().map(|row| row.name).collect();
    assert_eq!(
        declared_names.len(),
        declarations.len(),
        "duplicate static census rows"
    );
    assert_eq!(
        declared_names, discovered,
        "static schema census must cover every factory"
    );
    let packs: Vec<String> = expected.into_iter().collect();
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: packs.clone(),
        actor_id: None,
        brain_profile: None,
        ..RuntimeConfig::no_embeddings()
    })
    .unwrap();
    let mut builder = VerbRegistryBuilder::new();
    PackRegistry::register_packs(&packs, runtime, &mut builder).unwrap();
    let registry = builder.build_metadata().unwrap();
    let owners = registry.pack_names();
    let plans = registry.all_schema_plans_with_columns();
    assert_eq!(owners.len(), plans.len());
    assert_eq!(owners.iter().copied().collect::<BTreeSet<_>>(), discovered);
    let runtime_statements: usize = plans.iter().map(|(plan, _)| plan.statements.len()).sum();
    assert!(runtime_statements > 0, "non-vacuous actual plan census");
    assert!(
        plans.iter().any(|(_, columns)| !columns.is_empty()),
        "nullable upgrades included"
    );
    eprintln!("schema scope: {} factories, {} declarations, {runtime_statements} runtime statement batches", owners.len(), declarations.len());
    for migrated in [true, false] {
        let runtime_conn = scope_connection(migrated);
        for (owner, (plan, columns)) in owners.iter().zip(&plans) {
            if !plan.is_empty() {
                assert_eq!(*owner, plan.pack);
            }
            check_plan(&runtime_conn, owner, plan.statements, columns, !migrated)
                .unwrap_or_else(|e| panic!("registered {owner}, migrated={migrated}: {e}"));
        }
        let declared_conn = scope_connection(migrated);
        for row in &declarations {
            let statements = row.plan.as_ref().map_or(&[][..], |plan| {
                assert_eq!(plan.pack, row.name);
                plan.statements
            });
            check_plan(&declared_conn, row.name, statements, row.columns, !migrated)
                .unwrap_or_else(|e| panic!("declared {}, migrated={migrated}: {e}", row.name));
        }
    }
}

#[test]
fn sqlite_scope_guard_rejects_core_indexes_with_quoted_qualified_and_commented_names() {
    for table in CORE_TABLES {
        for sql in [
            format!("CREATE INDEX scope_probe ON {table}(id)"),
            format!("CREATE INDEX main.\"scope probe\" ON \"{}\"(id)", table.to_uppercase()),
            format!("CREATE /* before index */ INDEX IF NOT EXISTS [scope_probe] ON /* target */ `{table}`(id)"),
        ] {
            let conn = scope_connection(true);
            let error = check_statement(&conn, "fixture", &sql).unwrap_err();
            assert!(error.contains("core table"), "{table}: {error}");
            assert!(error.to_lowercase().contains(table), "{table}: {error}");
        }
    }
}

#[test]
fn scope_guard_rejects_legacy_if_not_exists_and_cold_core_table_creation() {
    let conn = scope_connection(true);
    let legacy = "CREATE INDEX IF NOT EXISTS idx_comm_message_direction ON notes(namespace, kind, json_extract(properties, '$.direction'), json_extract(properties, '$.read'), created_at DESC) WHERE deleted_at IS NULL";
    assert!(check_statement(&conn, "comm", legacy)
        .unwrap_err()
        .contains("core table notes"));
    for table in CORE_TABLES {
        let cold = scope_connection(false);
        let sql = format!("CREATE TABLE IF NOT EXISTS \"{table}\" (id TEXT)");
        assert!(check_statement(&cold, "fixture", &sql)
            .unwrap_err()
            .contains("core table"));
    }
}

#[test]
fn scope_guard_accepts_auxiliary_ddl_and_literal_core_names() {
    let conn = scope_connection(false);
    let sql = "CREATE TABLE auxiliary (id INTEGER PRIMARY KEY, body TEXT DEFAULT 'notes');
        CREATE INDEX aux_index ON auxiliary(body);
        CREATE VIRTUAL TABLE aux_fts USING fts5(body);
        CREATE TRIGGER aux_insert AFTER INSERT ON auxiliary BEGIN
            INSERT INTO aux_fts(rowid,body) VALUES(new.id,new.body); END;";
    check_statement(&conn, "fixture", sql).unwrap();
    conn.execute("INSERT INTO auxiliary(id) VALUES(1)", [])
        .unwrap();
    let text: String = conn
        .query_row("SELECT body FROM aux_fts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(text, "notes");
}

#[test]
fn nullable_column_scope_checks_precede_existing_column_noops() {
    let backend = khive_db::StorageBackend::memory().unwrap();
    backend
        .apply_pack_ddl_statements(&["CREATE TABLE auxiliary (id INTEGER PRIMARY KEY)"])
        .unwrap();
    let auxiliary = [PackColumnAddition {
        table: "auxiliary",
        column: "detail",
        affinity: PackColumnAffinity::Text,
    }];
    check_column_targets("fixture", &auxiliary).unwrap();
    backend
        .apply_pack_ddl_statements_with_columns(
            &["CREATE TABLE IF NOT EXISTS auxiliary (id INTEGER PRIMARY KEY, detail TEXT)"],
            &auxiliary,
        )
        .unwrap();
    let reader = backend.pool().reader().unwrap();
    check_column_shapes(reader.conn(), "fixture", &auxiliary).unwrap();
    for table in CORE_TABLES {
        let forbidden = [PackColumnAddition {
            table,
            column: "id",
            affinity: PackColumnAffinity::Text,
        }];
        assert!(check_column_targets("fixture", &forbidden)
            .unwrap_err()
            .contains("core table"));
    }
}

#[test]
fn tool_invalidation_exception_4137_is_byte_pinned_and_pack_scoped() {
    let sql = include_str!("../../khive-pack-tool/sql/grant-invalidation.sql");
    assert!(
        trusted_tool_batch("tool", sql),
        "review changed accepted tool SQL explicitly"
    );
    assert!(!trusted_tool_batch("other", sql));
    assert!(!trusted_tool_batch("tool", &format!("{sql}\n-- changed")));
    let changed = sql.replace("registry_insert", "registry_inseru");
    let core = scope_connection(true);
    let error = check_statement(&core, "tool", &changed).unwrap_err();
    assert!(error.contains("core table entities"), "{error}");
    assert!(error.contains("#4137"), "{error}");
    let create = AuthAction::CreateTrigger {
        trigger_name: TOOL_INVALIDATION_TRIGGER,
        table_name: "entities",
    };
    assert!(refused_action(create, true).is_none());
    assert!(refused_action(create, false).is_some());
    for action in [
        AuthAction::CreateIndex {
            index_name: "forbidden",
            table_name: "entities",
        },
        AuthAction::CreateIndex {
            index_name: "forbidden",
            table_name: "notes",
        },
        AuthAction::CreateTrigger {
            trigger_name: "another",
            table_name: "entities",
        },
        AuthAction::Read {
            table_name: "notes",
            column_name: "id",
        },
        AuthAction::Update {
            table_name: "entities",
            column_name: "name",
        },
    ] {
        assert!(
            refused_action(action, true).is_some(),
            "exception must not permit {action:?}"
        );
    }
}

#[test]
fn scope_guard_prepares_deferred_view_and_auxiliary_trigger_bodies() {
    for (setup, label) in [
        ("CREATE VIEW auxiliary_view AS SELECT id FROM notes", "view read"),
        ("CREATE TRIGGER auxiliary_insert AFTER INSERT ON auxiliary BEGIN UPDATE notes SET name='changed'; END", "insert trigger write"),
        ("CREATE TRIGGER auxiliary_update AFTER UPDATE OF body ON auxiliary BEGIN DELETE FROM notes; END", "update-of trigger write"),
        ("CREATE TRIGGER auxiliary_delete AFTER DELETE ON auxiliary BEGIN INSERT INTO notes(id,namespace,kind,created_at,updated_at) VALUES('x','local','message',1,1); END", "delete trigger write"),
        ("CREATE VIEW auxiliary_view AS SELECT id,body FROM auxiliary; CREATE TRIGGER view_insert INSTEAD OF INSERT ON auxiliary_view BEGIN UPDATE notes SET name='changed'; END", "instead-of view trigger write"),
    ] {
        let conn = scope_connection(true);
        conn.execute_batch("CREATE TABLE auxiliary (id INTEGER PRIMARY KEY, body TEXT)").unwrap();
        let error = check_plan(&conn, "fixture", &[setup], &[], false).unwrap_err();
        assert!(error.contains("core table notes"), "{label}: {error}");
    }
}

#[test]
fn deferred_checks_preserve_auxiliary_fts_generated_columns_and_partial_view_triggers() {
    let conn = scope_connection(false);
    let plan = [
        "CREATE TABLE auxiliary (id INTEGER PRIMARY KEY, body TEXT, generated_body TEXT GENERATED ALWAYS AS (body) STORED)",
        "CREATE VIRTUAL TABLE auxiliary_fts USING fts5(body)",
        "CREATE TRIGGER auxiliary_insert AFTER INSERT ON auxiliary BEGIN INSERT INTO auxiliary_fts(rowid,body) VALUES(new.id,new.body); END",
        "CREATE VIEW auxiliary_view AS SELECT id,body FROM auxiliary",
        "CREATE TRIGGER view_insert INSTEAD OF INSERT ON auxiliary_view BEGIN INSERT INTO auxiliary(id,body) VALUES(new.id,new.body); END",
    ];
    check_plan(&conn, "fixture", &plan, &[], true).unwrap();
    for table in ["auxiliary", "auxiliary_fts"] {
        let count: i64 = conn
            .query_row(
                &format!("SELECT count(*) FROM {}", quoted(table)),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 0,
            "deferred checks must only prepare, never write {table}"
        );
    }
    conn.execute("INSERT INTO auxiliary_view(id,body) VALUES(1,'kept')", [])
        .unwrap();
    let text: String = conn
        .query_row("SELECT body FROM auxiliary_fts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(text, "kept");
}

#[test]
fn temporary_trigger_and_view_plans_are_explicitly_outside_guard_scope() {
    for sql in [
        "CREATE TEMP VIEW hidden AS SELECT id FROM notes",
        "CREATE TEMP TRIGGER hidden AFTER INSERT ON auxiliary BEGIN UPDATE notes SET name='x'; END",
    ] {
        let conn = scope_connection(true);
        conn.execute_batch("CREATE TABLE auxiliary(id INTEGER)")
            .unwrap();
        let error = check_plan(&conn, "fixture", &[sql], &[], false).unwrap_err();
        assert!(
            error.contains("temporary trigger/view plans are not covered"),
            "{error}"
        );
    }
}
