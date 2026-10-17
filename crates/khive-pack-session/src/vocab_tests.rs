use std::collections::HashMap;

use khive_runtime::secret_gate::SESSION_MIRROR_STORED_TARGET;

use super::SESSION_SCHEMA_PLAN_STMTS;

/// Extract `(table_name, column_names)` from a `CREATE TABLE IF NOT
/// EXISTS <name> (<col1 TYPE ..., col2 TYPE ...>)` statement. `None` for
/// non-`CREATE TABLE` statements (the plan also carries `CREATE INDEX`).
/// Column definitions here never contain a comma inside a quoted
/// default, so splitting the body on `,` is exact for this schema.
fn parse_ddl_columns(stmt: &str) -> Option<(String, Vec<String>)> {
    const MARKER: &str = "CREATE TABLE IF NOT EXISTS ";
    if !stmt.starts_with(MARKER) {
        return None;
    }
    let rest = &stmt[MARKER.len()..];
    let open = rest.find('(')?;
    let close = rest.rfind(')')?;
    let table = rest[..open].trim().to_string();
    let columns = rest[open + 1..close]
        .split(',')
        .filter_map(|col| col.split_whitespace().next().map(str::to_string))
        .collect();
    Some((table, columns))
}

/// Regression: `SESSION_MIRROR_STORED_TARGET` (`khive-runtime`'s
/// executable ADR-115 contract) must name only columns that actually
/// exist in the session mirror schema below — a stored-target string
/// naming a nonexistent column silently overstates the masking
/// guarantee. Fails before the fix on `session_messages.cwd` and
/// `session_messages.git_branch`, neither of which is a real column
/// (`cwd`/`git_branch` live only on `sessions`).
#[test]
fn session_mirror_stored_target_names_only_real_columns() {
    let mut tables: HashMap<String, Vec<String>> = HashMap::new();
    for stmt in &SESSION_SCHEMA_PLAN_STMTS {
        if let Some((table, columns)) = parse_ddl_columns(stmt) {
            tables.insert(table, columns);
        }
    }
    assert!(
        tables.contains_key("sessions") && tables.contains_key("session_messages"),
        "DDL parser must find both tables in the schema plan: {tables:?}"
    );

    for entry in SESSION_MIRROR_STORED_TARGET.split(',') {
        let entry = entry
            .trim()
            .trim_start_matches("and ")
            .trim_end_matches('.');
        let (table, column) = entry
            .split_once('.')
            .unwrap_or_else(|| panic!("stored-target entry {entry:?} is not table.column shaped"));
        let columns = tables.get(table).unwrap_or_else(|| {
            panic!("{table} named in SESSION_MIRROR_STORED_TARGET is not a real table")
        });
        assert!(
            columns.iter().any(|c| c == column),
            "{table}.{column} named in SESSION_MIRROR_STORED_TARGET does not exist; \
                 real {table} columns: {columns:?}"
        );
    }
}
