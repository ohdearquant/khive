use khive_types::{PackColumnAddition, PackColumnAffinity};
use rusqlite::{Connection, OptionalExtension};

use crate::SqliteError;

fn validate_identifier(identifier: &str) -> Result<(), SqliteError> {
    let mut bytes = identifier.bytes();
    if !bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(SqliteError::InvalidData(format!(
            "invalid pack schema identifier {identifier:?}"
        )));
    }
    Ok(())
}

fn declared_type(affinity: PackColumnAffinity) -> &'static str {
    match affinity {
        PackColumnAffinity::Text => "TEXT",
        PackColumnAffinity::Integer => "INTEGER",
    }
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool, SqliteError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM main.sqlite_schema \
         WHERE type = 'table' AND name = ?1 COLLATE NOCASE)",
        [table],
        |row| row.get(0),
    )?)
}

fn column_exists_and_matches(
    conn: &Connection,
    addition: &PackColumnAddition,
) -> Result<bool, SqliteError> {
    let column = conn
        .query_row(
            "SELECT type, \"notnull\", dflt_value, pk, hidden \
             FROM pragma_table_xinfo(?1, 'main') WHERE name = ?2 COLLATE NOCASE",
            [addition.table, addition.column],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((sql_type, not_null, default, primary_key, hidden)) = column else {
        return Ok(false);
    };
    let expected_type = declared_type(addition.affinity);
    if !sql_type.trim().eq_ignore_ascii_case(expected_type)
        || not_null != 0
        || default.is_some()
        || primary_key != 0
        || hidden != 0
    {
        return Err(SqliteError::InvalidData(format!(
            "incompatible pack schema column {}.{}: expected nullable {expected_type} \
             with no default, primary key, or hidden/generated value; found \
             type={sql_type:?}, notnull={not_null}, default={default:?}, \
             pk={primary_key}, hidden={hidden}",
            addition.table, addition.column,
        )));
    }
    Ok(true)
}

pub(super) fn add_missing_columns(
    conn: &Connection,
    additions: &[PackColumnAddition],
) -> Result<(), SqliteError> {
    for addition in additions {
        validate_identifier(addition.table)?;
        validate_identifier(addition.column)?;
    }
    for addition in additions {
        if table_exists(conn, addition.table)? && !column_exists_and_matches(conn, addition)? {
            let sql = format!(
                "ALTER TABLE main.\"{}\" ADD COLUMN \"{}\" {}",
                addition.table,
                addition.column,
                declared_type(addition.affinity),
            );
            conn.execute_batch(&sql)?;
        }
    }
    Ok(())
}

pub(super) fn validate_columns(
    conn: &Connection,
    additions: &[PackColumnAddition],
) -> Result<(), SqliteError> {
    for addition in additions {
        if !table_exists(conn, addition.table)? || !column_exists_and_matches(conn, addition)? {
            return Err(SqliteError::InvalidData(format!(
                "pack schema plan did not create declared column {}.{}",
                addition.table, addition.column,
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
