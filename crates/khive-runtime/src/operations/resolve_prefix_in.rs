use khive_storage::{SqlRow, SqlStatement, SqlValue};
use uuid::Uuid;

use super::uuid_prefix_bounds;
use crate::{KhiveRuntime, RuntimeError, RuntimeResult};

/// The rows of a pack-owned table that [`KhiveRuntime::resolve_prefix_in`] considers.
///
/// Both predicates are explicit, so every caller states its selection policy where it
/// resolves. They apply before candidates are limited, so a row outside the scope can
/// neither match a prefix nor make one ambiguous. A predicate naming a column the table
/// lacks is a storage error, never an unfiltered read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrefixScope<'a> {
    /// Only rows whose `namespace` column equals this value. `None` reads every
    /// namespace, for by-ID paths whose authorization is enforced elsewhere.
    pub namespace: Option<&'a str>,
    /// Exclude rows whose `deleted_at` column is set.
    pub live_only: bool,
}

impl KhiveRuntime {
    /// Resolve a UUID prefix in one pack-owned table, within `scope`.
    ///
    /// This resolves identifiers, not authorization. `scope` selects rows by
    /// namespace and deletion state; any other selection predicate (status, owner)
    /// stays with the caller.
    ///
    /// `table` and `id_column` are static because they name pack schema, never
    /// request input; they must also be ASCII identifiers matching
    /// `[A-Za-z_][A-Za-z0-9_]*`. The column must store canonical lowercase UUID
    /// text under BINARY collation. Duplicate rows holding the same ID count once.
    /// Compact hexadecimal and canonically dashed prefixes are accepted, in
    /// either case; malformed prefixes return `None` without querying storage.
    /// This low-level primitive imposes no minimum prefix length.
    ///
    /// A full UUID is looked up, including its compact or uppercase spelling;
    /// an absent full UUID returns `None`. Unlike [`Self::resolve_uuid_or_prefix`],
    /// this method does not pass full UUIDs through without checking the table.
    /// Missing tables/columns and backend failures remain storage errors, malformed
    /// stored UUIDs are internal errors, and multiple distinct matches return
    /// [`RuntimeError::AmbiguousPrefix`].
    pub async fn resolve_prefix_in(
        &self,
        table: &'static str,
        id_column: &'static str,
        scope: PrefixScope<'_>,
        prefix: &str,
    ) -> RuntimeResult<Option<Uuid>> {
        let Some(statement) = prefix_statement(table, id_column, scope, prefix)? else {
            return Ok(None);
        };
        let mut reader = self.sql().reader().await?;
        decode_prefix_matches(prefix, reader.query_all(statement).await?)
    }
}

fn quoted_identifier(identifier: &str, kind: &str) -> RuntimeResult<String> {
    let mut bytes = identifier.bytes();
    if !bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(RuntimeError::InvalidInput(format!(
            "resolve_prefix_in: invalid {kind} identifier"
        )));
    }
    Ok(format!("\"{identifier}\""))
}

fn prefix_statement(
    table: &str,
    id_column: &str,
    scope: PrefixScope<'_>,
    prefix: &str,
) -> RuntimeResult<Option<SqlStatement>> {
    let table = quoted_identifier(table, "table")?;
    let id_column = quoted_identifier(id_column, "column")?;
    let Some((lower, upper)) = uuid_prefix_bounds(prefix) else {
        return Ok(None);
    };
    let mut params = vec![SqlValue::Text(lower), SqlValue::Text(upper)];
    let mut filters = String::new();
    if let Some(namespace) = scope.namespace {
        params.push(SqlValue::Text(namespace.to_owned()));
        filters.push_str(" AND candidate.namespace = ?3");
    }
    if scope.live_only {
        filters.push_str(" AND candidate.deleted_at IS NULL");
    }
    Ok(Some(
        SqlStatement::new(
            format!(
                "SELECT DISTINCT candidate.{id_column} AS resolved_id FROM {table} AS candidate \
                 WHERE candidate.{id_column} >= ?1 AND candidate.{id_column} < ?2{filters} \
                 ORDER BY candidate.{id_column} LIMIT 2"
            ),
            params,
        )
        .labelled("resolve_prefix_in"),
    ))
}

fn decode_prefix_matches(prefix: &str, rows: Vec<SqlRow>) -> RuntimeResult<Option<Uuid>> {
    let matches = rows
        .iter()
        .map(|row| {
            row.uuid("resolved_id")
                .map_err(|error| RuntimeError::Internal(format!("stored UUID is invalid: {error}")))
        })
        .collect::<RuntimeResult<Vec<_>>>()?;
    match matches.as_slice() {
        [] => Ok(None),
        [id] => Ok(Some(*id)),
        _ => Err(RuntimeError::AmbiguousPrefix {
            prefix: prefix.to_owned(),
            matches,
        }),
    }
}

#[cfg(test)]
mod tests {
    use khive_storage::types::SqlColumn;

    use super::*;

    #[test]
    fn mistyped_or_missing_result_column_is_an_error() {
        for columns in [
            vec![],
            vec![SqlColumn {
                name: "resolved_id".into(),
                value: SqlValue::Integer(1),
            }],
        ] {
            assert!(matches!(
                decode_prefix_matches("aabbccdd", vec![SqlRow { columns }]),
                Err(RuntimeError::Internal(_))
            ));
        }
    }
}
