use super::*;

/// Drop per-namespace FTS5 partition tables (`fts_entities_*`, `fts_notes_*`) that
/// may exist in databases that were not yet migrated or were created before V4.
/// Canonical tables (`fts_entities`, `fts_notes`, `fts_knowledge`, `fts_sections`)
/// and their FTS5 shadow tables and canonical rowid companions are never dropped.
/// Safe to run repeatedly; a no-op on fresh databases.
///
/// **Sweep guard**: only drops partition tables when every distinct namespace
/// present in the base `entities`/`notes` tables was covered by this reindex
/// pass (i.e. the operating namespace `covered_ns` is the only namespace in the
/// base). If uncovered namespaces exist, the sweep is skipped and a warning is
/// emitted so operators know a manual or multi-namespace reindex is needed.
pub(super) async fn sweep_stale_fts_partitions(rt: &KhiveRuntime, covered_ns: &str) {
    use khive_db::stores::text::{rowid_map_state_table, rowid_map_table};
    use khive_storage::types::{SqlStatement, SqlValue};

    // Guard: only sweep when every distinct namespace present in base
    // entities/notes was covered by this reindex pass. A single-namespace
    // (post-relabel) db has exactly {covered_ns} and passes immediately. A
    // multi-namespace db would be partially swept — rows in other namespaces
    // were dropped from old partitions but never carried to the unified table —
    // so we skip and warn instead.
    let base_namespaces = distinct_base_namespaces(rt).await;
    let uncovered: Vec<&str> = base_namespaces
        .iter()
        .filter(|ns| ns.as_str() != covered_ns)
        .map(String::as_str)
        .collect();
    if !uncovered.is_empty() {
        tracing::warn!(
            covered = covered_ns,
            uncovered = ?uncovered,
            "skipping stale FTS partition sweep: base tables contain namespaces not \
             covered by this reindex pass; run reindex for each namespace first, \
             or normalize all rows to one namespace before sweeping"
        );
        return;
    }

    // Canonical base names that must never be dropped.
    let canonical: &[&str] = &["fts_entities", "fts_notes", "fts_knowledge", "fts_sections"];
    let canonical_companions = ["fts_entities", "fts_notes"]
        .into_iter()
        .flat_map(|table| [rowid_map_table(table), rowid_map_state_table(table)])
        .collect::<HashSet<_>>();

    // FTS5 shadow table suffixes that must never be dropped (the extension drops
    // them automatically when the virtual table itself is dropped; we only drop
    // the virtual table, so these patterns must be excluded from discovery).
    let shadow_suffixes: &[&str] = &["_data", "_idx", "_docsize", "_config", "_content"];

    let sql = rt.sql();
    let Ok(mut reader) = sql.reader().await else {
        return;
    };

    // Find candidate tables: type='table', name starts with `fts_entities_` or `fts_notes_`.
    let rows = reader
        .query_all(SqlStatement {
            sql: sql!("fts_partitions_list_stale").into(),
            params: vec![],
            label: Some("sweep_stale_fts_partitions_discover".into()),
        })
        .await;

    let rows = match rows {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "failed to discover stale FTS partition tables");
            return;
        }
    };

    let mut to_drop: Vec<String> = Vec::new();
    for row in &rows {
        let name = match row.get("name") {
            Some(SqlValue::Text(s)) => s.clone(),
            _ => continue,
        };
        // The prefix discovery also finds the live rowid maps and their state.
        if canonical.contains(&name.as_str()) || canonical_companions.contains(&name) {
            continue;
        }
        // Skip FTS5 shadow tables (they are dropped automatically with the virtual table).
        if shadow_suffixes.iter().any(|suf| name.ends_with(suf)) {
            continue;
        }
        to_drop.push(name);
    }
    drop(reader);

    if to_drop.is_empty() {
        return;
    }

    let Ok(mut writer) = sql.writer().await else {
        return;
    };
    for table in &to_drop {
        let ddl = format!(
            concat!("DROP ", "TABLE IF EXISTS {}"),
            quote_sqlite_identifier(table)
        );
        match writer
            .execute(SqlStatement {
                sql: ddl,
                params: vec![],
                label: Some("sweep_stale_fts_partitions_drop".into()),
            })
            .await
        {
            Ok(_) => {
                tracing::info!(table, "dropped stale FTS partition table");
            }
            Err(e) => {
                tracing::warn!(error = %e, table, "failed to drop stale FTS partition table");
            }
        }
    }
}
