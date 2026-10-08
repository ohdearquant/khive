use super::*;

/// Which rows contribute to administrative namespace enumeration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NamespaceLiveness {
    /// Exclude tombstoned entities and notes. Events are append-only.
    #[default]
    Live,
    /// Include tombstoned entities and notes as well as all events.
    All,
}

impl StorageBackend {
    /// List sorted, distinct namespaces containing an exact stored record kind.
    ///
    /// This administrative read is unscoped: it unions existing entities, notes
    /// and events tables, comparing `kind` exactly. It does not initialize absent
    /// tables, filter schedules by due time, or interpret kind as a substrate.
    /// Unknown kinds return an empty list. Pass `NamespaceLiveness::default()`
    /// for live rows; events have no tombstone field, so both modes include them.
    pub async fn list_namespaces(
        &self,
        kind: &str,
        liveness: NamespaceLiveness,
    ) -> khive_storage::StorageResult<Vec<String>> {
        let kind = kind.to_string();
        crate::stores::run_pooled_store_read(
            Arc::clone(&self.pool),
            khive_storage::StorageCapability::Sql,
            "list_namespaces",
            move |conn| {
                let read = || -> Result<Vec<String>, SqliteError> {
                    let mut namespaces = std::collections::BTreeSet::new();
                    // Tables are optional, and only these fixed identifiers enter SQL.
                    for (table, has_tombstones) in
                        [("entities", true), ("notes", true), ("events", false)]
                    {
                        if !sqlite_table_exists(conn, table)? {
                            continue;
                        }
                        let live = if has_tombstones && liveness == NamespaceLiveness::Live {
                            " AND deleted_at IS NULL"
                        } else {
                            ""
                        };
                        let mut statement = conn.prepare(&format!(
                            "SELECT DISTINCT namespace FROM {table} WHERE kind = ?1{live}"
                        ))?;
                        let rows = statement
                            .query_map(rusqlite::params![kind], |row| row.get::<_, String>(0))?;
                        for row in rows {
                            namespaces.insert(row?);
                        }
                    }
                    Ok(namespaces.into_iter().collect())
                };
                read().map_err(|error| {
                    error.into_storage_error(
                        khive_storage::StorageCapability::Sql,
                        "list_namespaces",
                    )
                })
            },
        )
        .await
    }
}
