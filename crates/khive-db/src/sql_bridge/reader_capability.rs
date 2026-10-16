use super::{
    execute_prepared_query, execute_prepared_query_page, execute_prepared_query_row,
    map_rusqlite_err, next_sqlite_token, prepare_bound_statement, skip_sqlite_empty_prefix,
    transaction_control_head, CachedReadTransactionControl, PageRequest, SqlRow, SqlStatement,
    StorageCapability, StorageError,
};

/// Admit only SELECT and WITH followed by SELECT for enlisted graph guards.
/// SQLite still validates syntax and the executable tail during preparation.
/// This is a statement-shape check, not a sandbox against caller-provided UDFs.
pub(crate) fn statement_guard_select_admits(sql: &str) -> Result<(), String> {
    let rest = skip_sqlite_empty_prefix(sql.as_bytes());
    let admitted = match next_sqlite_token(rest) {
        Some((head, _)) if head.eq_ignore_ascii_case(b"SELECT") => true,
        Some((head, tail)) if head.eq_ignore_ascii_case(b"WITH") => {
            skip_common_table_expressions(tail)
                .and_then(next_sqlite_token)
                .is_some_and(|(head, _)| head.eq_ignore_ascii_case(b"SELECT"))
        }
        _ => false,
    };
    if admitted {
        Ok(())
    } else {
        Err("graph statement guards require SELECT or WITH followed by SELECT".into())
    }
}

/// SQLite's prepared-statement classifier is authoritative for the safety
/// boundary. Row-producing DML (`UPDATE ... RETURNING`) and transaction
/// control may be called through `SqlReader`, but they are admitted writes and
/// must never register an interrupt target.
fn statement_is_cancellable_read(stmt: &rusqlite::Statement<'_>, sql: &str) -> bool {
    stmt.readonly() && transaction_control_head(sql).is_none()
}

/// Introspection `PRAGMA`s admitted through the pooled reader capability with
/// an optional single positional argument (`PRAGMA name` or `PRAGMA
/// name(arg)`) — none of these has a set/assignment form in SQLite, so an
/// argument is always a read-side filter, never a mutation.
const READER_STRUCTURAL_PRAGMAS: [&str; 8] = [
    "table_info",
    "table_xinfo",
    "table_list",
    "index_list",
    "index_info",
    "index_xinfo",
    "foreign_key_list",
    "integrity_check",
];

/// Introspection `PRAGMA`s admitted through the pooled reader capability only
/// in their bare, argument-less read form. Every one of these also has a
/// `PRAGMA name = value` or `PRAGMA name(value)` assignment form in SQLite —
/// an assigning form changes connection-local state that a pristine-state
/// scan run only on dirty checkouts must never let back into the pool
/// unnoticed (`reader_connection_state_is_pristine`,
/// `reader_connection_settings_match_baseline`).
const READER_SETTING_PRAGMAS: [&str; 10] = [
    "database_list",
    "collation_list",
    "function_list",
    "compile_options",
    "page_count",
    "freelist_count",
    "user_version",
    "schema_version",
    "journal_mode",
    "page_size",
];

/// Skip a fully parenthesized region starting at `rest[0] == b'('`,
/// respecting SQL string/identifier quoting (`'...'` with `''` escapes,
/// `"..."`/`` `...` `` with doubled-quote escapes, and `[...]` bracket
/// identifiers) and line/block comments, so that a `)`, `(`, or `--` that
/// merely appears inside a literal or comment is never mistaken for syntax.
/// Returns the bytes after the matching `)`, or `None` if the region never
/// closes (malformed or truncated SQL) — callers must fail closed on `None`.
fn skip_balanced_parens(rest: &[u8]) -> Option<&[u8]> {
    debug_assert_eq!(rest.first(), Some(&b'('));
    let mut depth: u32 = 0;
    let mut idx = 0;
    loop {
        match *rest.get(idx)? {
            b'(' => {
                depth += 1;
                idx += 1;
            }
            b')' => {
                depth -= 1;
                idx += 1;
                if depth == 0 {
                    return Some(&rest[idx..]);
                }
            }
            quote @ (b'\'' | b'"' | b'`') => {
                idx += 1;
                loop {
                    match *rest.get(idx)? {
                        byte if byte == quote => {
                            idx += 1;
                            if rest.get(idx) == Some(&quote) {
                                idx += 1; // doubled-quote escape inside the literal
                            } else {
                                break;
                            }
                        }
                        _ => idx += 1,
                    }
                }
            }
            b'[' => {
                idx += 1;
                while *rest.get(idx)? != b']' {
                    idx += 1;
                }
                idx += 1;
            }
            b'-' if rest.get(idx + 1) == Some(&b'-') => {
                idx += 2;
                while idx < rest.len() && rest[idx] != b'\n' {
                    idx += 1;
                }
            }
            b'/' if rest.get(idx + 1) == Some(&b'*') => {
                idx += 2;
                while idx + 1 < rest.len() && !(rest[idx] == b'*' && rest[idx + 1] == b'/') {
                    idx += 1;
                }
                idx = (idx + 2).min(rest.len());
            }
            _ => idx += 1,
        }
    }
}

/// Skip one SQLite identifier — unquoted (`[A-Za-z0-9_]+`, as
/// [`next_sqlite_token`] already recognizes), double-quoted or
/// backtick-quoted (both honoring the doubled-quote-character escape), or
/// bracket-quoted (no escape; SQLite's bracket form ends at the first `]`) —
/// and return the bytes after it. A quoted CTE name may contain any byte,
/// including `(` and `)`, so the CTE-list walk must skip the identifier
/// itself rather than tokenizing it the way an unquoted keyword is.
fn skip_sqlite_identifier(rest: &[u8]) -> Option<&[u8]> {
    match *rest.first()? {
        quote @ (b'"' | b'`') => {
            let mut idx = 1;
            loop {
                match *rest.get(idx)? {
                    byte if byte == quote => {
                        idx += 1;
                        if rest.get(idx) == Some(&quote) {
                            idx += 1; // doubled-quote escape inside the identifier
                        } else {
                            break;
                        }
                    }
                    _ => idx += 1,
                }
            }
            Some(&rest[idx..])
        }
        b'[' => {
            let mut idx = 1;
            while *rest.get(idx)? != b']' {
                idx += 1;
            }
            Some(&rest[idx + 1..])
        }
        _ => next_sqlite_token(rest).map(|(_, next)| next),
    }
}

/// Walk past the common-table-expression list following `WITH [RECURSIVE]`
/// and return the bytes starting at the main statement's own head keyword.
/// Each CTE body is skipped as a balanced parenthesized region
/// ([`skip_balanced_parens`]), so nested parens, string literals, and
/// comments inside a CTE body never confuse the walk. Returns `None` if the
/// CTE list is not well-formed enough to walk past safely — the caller must
/// fail closed (refuse admission) rather than guess.
fn skip_common_table_expressions(tail: &[u8]) -> Option<&[u8]> {
    let mut rest = skip_sqlite_empty_prefix(tail);
    if let Some((word, next)) = next_sqlite_token(rest) {
        if word.eq_ignore_ascii_case(b"RECURSIVE") {
            rest = skip_sqlite_empty_prefix(next);
        }
    }
    loop {
        // CTE name — unquoted or SQLite-quoted (`"..."`, `` `...` ``, `[...]`).
        let next = skip_sqlite_identifier(rest)?;
        rest = skip_sqlite_empty_prefix(next);
        // Optional column-name list.
        if rest.first() == Some(&b'(') {
            rest = skip_sqlite_empty_prefix(skip_balanced_parens(rest)?);
        }
        let (as_keyword, next) = next_sqlite_token(rest)?;
        if !as_keyword.eq_ignore_ascii_case(b"AS") {
            return None;
        }
        rest = skip_sqlite_empty_prefix(next);
        // Optional `[NOT] MATERIALIZED` hint (SQLite 3.35+).
        if let Some((word, next)) = next_sqlite_token(rest) {
            if word.eq_ignore_ascii_case(b"MATERIALIZED") {
                rest = skip_sqlite_empty_prefix(next);
            } else if word.eq_ignore_ascii_case(b"NOT") {
                let (materialized, next) = next_sqlite_token(skip_sqlite_empty_prefix(next))?;
                if !materialized.eq_ignore_ascii_case(b"MATERIALIZED") {
                    return None;
                }
                rest = skip_sqlite_empty_prefix(next);
            }
        }
        // CTE body.
        if rest.first() != Some(&b'(') {
            return None;
        }
        rest = skip_sqlite_empty_prefix(skip_balanced_parens(rest)?);
        if rest.first() == Some(&b',') {
            rest = skip_sqlite_empty_prefix(&rest[1..]);
            continue;
        }
        return Some(rest);
    }
}

/// Reject any raw SQL that is not one of a small allow-listed set of
/// read-only statement shapes before it ever reaches a pooled reader
/// connection.
///
/// `stmt.readonly()` (SQLite's own classifier, used by
/// [`statement_is_cancellable_read`] to decide interrupt eligibility) is not
/// a sufficient admission predicate on its own: by SQLite's own definition it
/// also returns `true` for `ATTACH`/`DETACH`, `CREATE TEMP TABLE`, and
/// configuration `PRAGMA`s like `writable_schema`, `busy_timeout`, or
/// `cache_size` — none of which write the main database file, but all of
/// which leave connection-local state that persists across pooled checkouts.
/// Classification here instead runs on the statement head, admitting only
/// `SELECT`, `WITH ... SELECT`, `VALUES`, `EXPLAIN [QUERY PLAN] <admitted>`,
/// and the fixed `PRAGMA` allow-lists above. A `WITH` clause is not itself a
/// read shape — SQLite also allows `WITH ... INSERT/UPDATE/DELETE`, with or
/// without `RETURNING` — so a `WITH` head is admitted only after walking past
/// its CTE list ([`skip_common_table_expressions`]) and confirming the main
/// statement underneath is itself `SELECT` or `VALUES`.
pub(crate) fn reader_capability_admits(sql: &str) -> Result<(), String> {
    let rest = skip_sqlite_empty_prefix(sql.as_bytes());
    let Some((head, tail)) = next_sqlite_token(rest) else {
        // No head token (empty/comment-only statement): let SQLite's own
        // prepare step surface that error rather than duplicating it here.
        return Ok(());
    };
    if head.eq_ignore_ascii_case(b"SELECT") || head.eq_ignore_ascii_case(b"VALUES") {
        return Ok(());
    }
    if head.eq_ignore_ascii_case(b"WITH") {
        let Some(after_ctes) = skip_common_table_expressions(tail) else {
            return Err(
                "WITH statement's common-table-expression list could not be parsed; refusing \
                 to admit it through the reader capability"
                    .into(),
            );
        };
        return match next_sqlite_token(after_ctes) {
            Some((main_head, _))
                if main_head.eq_ignore_ascii_case(b"SELECT")
                    || main_head.eq_ignore_ascii_case(b"VALUES") =>
            {
                Ok(())
            }
            other => Err(format!(
                "WITH ... {:?} is not admitted through the reader capability; only a \
                 read-only SELECT/VALUES body after the CTE list may run against a pooled \
                 reader connection",
                other.map_or_else(
                    || "<none>".to_string(),
                    |(main_head, _)| String::from_utf8_lossy(main_head).into_owned()
                )
            )),
        };
    }
    if head.eq_ignore_ascii_case(b"EXPLAIN") {
        let mut rest = skip_sqlite_empty_prefix(tail);
        if let Some((query, next)) = next_sqlite_token(rest) {
            if query.eq_ignore_ascii_case(b"QUERY") {
                let after_query = skip_sqlite_empty_prefix(next);
                match next_sqlite_token(after_query) {
                    Some((plan, next2)) if plan.eq_ignore_ascii_case(b"PLAN") => {
                        rest = skip_sqlite_empty_prefix(next2);
                    }
                    _ => {
                        return Err(
                            "EXPLAIN QUERY must be followed by PLAN through the reader capability"
                                .into(),
                        );
                    }
                }
            }
        }
        return reader_capability_admits(&String::from_utf8_lossy(rest));
    }
    if head.eq_ignore_ascii_case(b"PRAGMA") {
        return reader_capability_admits_pragma(tail);
    }
    Err(format!(
        "statement head {:?} is not admitted through the reader capability; only \
         SELECT/WITH/VALUES/EXPLAIN and an allow-listed set of read-only PRAGMA forms \
         may run against a pooled reader connection",
        String::from_utf8_lossy(head)
    ))
}

fn reader_capability_admits_pragma(tail: &[u8]) -> Result<(), String> {
    let rest = skip_sqlite_empty_prefix(tail);
    let Some((mut name, mut after_name)) = next_sqlite_token(rest) else {
        return Err("PRAGMA with no name is not admitted through the reader capability".into());
    };
    // `schema.pragma_name` — the schema qualifier changes which attached
    // database the pragma targets, not which pragma runs.
    if after_name.first() == Some(&b'.') {
        let (qualified_name, qualified_after) =
            next_sqlite_token(&after_name[1..]).ok_or_else(|| {
                "PRAGMA schema-qualifier with no pragma name is not admitted through the \
                 reader capability"
                    .to_string()
            })?;
        name = qualified_name;
        after_name = qualified_after;
    }
    let after = skip_sqlite_empty_prefix(after_name);
    let is_structural = READER_STRUCTURAL_PRAGMAS
        .iter()
        .any(|allowed| name.eq_ignore_ascii_case(allowed.as_bytes()));
    let is_setting = READER_SETTING_PRAGMAS
        .iter()
        .any(|allowed| name.eq_ignore_ascii_case(allowed.as_bytes()));
    if !is_structural && !is_setting {
        return Err(format!(
            "PRAGMA {:?} is not admitted through the reader capability",
            String::from_utf8_lossy(name)
        ));
    }
    if after.first() == Some(&b'=') {
        return Err(format!(
            "PRAGMA {:?} may not be assigned through the reader capability",
            String::from_utf8_lossy(name)
        ));
    }
    if after.first() == Some(&b'(') && !is_structural {
        return Err(format!(
            "PRAGMA {:?} may not carry an argument through the reader capability",
            String::from_utf8_lossy(name)
        ));
    }
    Ok(())
}

/// Refuse a statement bound for the reader capability that is neither
/// admitted transaction control (handled by the caller's cached
/// read-transaction state machine) nor an admitted read shape
/// ([`reader_capability_admits`]).
pub(super) fn admit_reader_capability_sql(
    statement: &SqlStatement,
    transaction_control: Option<CachedReadTransactionControl>,
    operation: &'static str,
) -> khive_storage::types::StorageResult<()> {
    // Only the admitted single-level deferred-read span (`BEGIN`/`BEGIN
    // DEFERRED` and its `COMMIT`/`END`/`ROLLBACK` counterpart) bypasses the
    // read-shape check below; `Unsupported` forms (`BEGIN IMMEDIATE`,
    // `SAVEPOINT`, `ROLLBACK TO ...`) fall through and are refused there like
    // any other non-admitted statement head.
    if matches!(
        transaction_control,
        Some(CachedReadTransactionControl::BeginDeferred)
            | Some(CachedReadTransactionControl::Finish(_))
    ) {
        return Ok(());
    }
    reader_capability_admits(&statement.sql).map_err(|message| StorageError::InvalidInput {
        capability: StorageCapability::Sql,
        operation: operation.into(),
        message,
    })
}

pub(super) fn execute_query_interruptibly(
    scope: &crate::read_cancellation::InterruptibleReadScope,
    conn: &rusqlite::Connection,
    statement: &SqlStatement,
    operation: &'static str,
    rollback_interrupted_transaction: bool,
    interruptible: bool,
) -> khive_storage::types::StorageResult<Vec<SqlRow>> {
    let stmt = prepare_bound_statement(conn, statement)
        .map_err(|error| map_rusqlite_err(error, operation))?;
    if interruptible && statement_is_cancellable_read(&stmt, &statement.sql) {
        scope.run_with_interrupted_cleanup(
            conn,
            move || {
                execute_prepared_query(stmt).map_err(|error| map_rusqlite_err(error, operation))
            },
            || {
                rollback_interrupted_read_transaction(
                    conn,
                    operation,
                    rollback_interrupted_transaction,
                )
            },
        )
    } else {
        scope.mark_write_committed()?;
        execute_prepared_query(stmt).map_err(|error| map_rusqlite_err(error, operation))
    }
}

pub(super) fn execute_query_row_interruptibly(
    scope: &crate::read_cancellation::InterruptibleReadScope,
    conn: &rusqlite::Connection,
    statement: &SqlStatement,
    operation: &'static str,
    rollback_interrupted_transaction: bool,
    interruptible: bool,
) -> khive_storage::types::StorageResult<Option<SqlRow>> {
    let stmt = prepare_bound_statement(conn, statement)
        .map_err(|error| map_rusqlite_err(error, operation))?;
    if interruptible && statement_is_cancellable_read(&stmt, &statement.sql) {
        scope.run_with_interrupted_cleanup(
            conn,
            move || {
                execute_prepared_query_row(stmt).map_err(|error| map_rusqlite_err(error, operation))
            },
            || {
                rollback_interrupted_read_transaction(
                    conn,
                    operation,
                    rollback_interrupted_transaction,
                )
            },
        )
    } else {
        scope.mark_write_committed()?;
        execute_prepared_query_row(stmt).map_err(|error| map_rusqlite_err(error, operation))
    }
}

pub(super) fn execute_query_page_interruptibly(
    scope: &crate::read_cancellation::InterruptibleReadScope,
    conn: &rusqlite::Connection,
    statement: &SqlStatement,
    page: &PageRequest,
    operation: &'static str,
    rollback_interrupted_transaction: bool,
    interruptible: bool,
) -> khive_storage::types::StorageResult<Vec<SqlRow>> {
    let stmt = prepare_bound_statement(conn, statement)
        .map_err(|error| map_rusqlite_err(error, operation))?;
    if interruptible && statement_is_cancellable_read(&stmt, &statement.sql) {
        scope.run_with_interrupted_cleanup(
            conn,
            move || {
                execute_prepared_query_page(stmt, page)
                    .map_err(|error| map_rusqlite_err(error, operation))
            },
            || {
                rollback_interrupted_read_transaction(
                    conn,
                    operation,
                    rollback_interrupted_transaction,
                )
            },
        )
    } else {
        scope.mark_write_committed()?;
        execute_prepared_query_page(stmt, page).map_err(|error| map_rusqlite_err(error, operation))
    }
}

fn rollback_interrupted_read_transaction(
    conn: &rusqlite::Connection,
    operation: &'static str,
    enabled: bool,
) -> khive_storage::types::StorageResult<()> {
    if !enabled || conn.is_autocommit() {
        return Ok(());
    }
    conn.execute_batch("ROLLBACK")
        .map_err(|error| map_rusqlite_err(error, operation))?;
    if conn.is_autocommit() {
        Ok(())
    } else {
        Err(StorageError::Transaction {
            operation: operation.into(),
            message: "interrupted read transaction rollback did not restore autocommit".into(),
        })
    }
}
