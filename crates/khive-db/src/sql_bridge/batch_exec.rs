//! Prepared batch execution and transaction-control classification.

use khive_storage::error::StorageError;
use khive_storage::types::SqlStatement;
use khive_storage::StorageCapability;

use super::rows::{bind_params, prepare_sql_statement, AtomicEventRows, PreparedBatchStatement};

/// Bind and execute handles returned by [`prepare_batch_statements`](super::rows::prepare_batch_statements).
pub(super) fn execute_prepared_batch<'conn>(
    conn: &'conn rusqlite::Connection,
    prepared: Vec<PreparedBatchStatement<'conn>>,
    statements: &[SqlStatement],
    event_rows: Option<&AtomicEventRows>,
) -> Result<u64, rusqlite::Error> {
    debug_assert_eq!(prepared.len(), statements.len());
    let mut total = 0u64;
    for (prepared, statement) in prepared.into_iter().zip(statements) {
        let mut prepared = match prepared {
            PreparedBatchStatement::Ready(prepared) => prepared,
            PreparedBatchStatement::PrepareAtExecution => {
                prepare_sql_statement(conn, &statement.sql)?
            }
        };
        bind_params(&mut prepared, &statement.params)?;
        let affected = prepared.raw_execute()? as u64;
        if let Some(event_rows) = event_rows {
            event_rows.observe(statement, affected);
        }
        total += affected;
    }
    Ok(total)
}

/// SQL statement heads that are transaction control. `execute_batch` owns the
/// `BEGIN`/`COMMIT` boundary for the whole batch (the standalone path wraps
/// the list in its own `BEGIN IMMEDIATE`, and the queue-backed path runs
/// inside the writer task's per-request transaction), so a caller-supplied
/// statement that itself starts, ends, or branches a transaction can commit
/// or roll back early and break the batch's all-or-nothing contract. Cached
/// read-only handles use the same lexical classification to drive their
/// separately admitted single-level read-transaction state machine below.
/// `START` is classified as the alternate transaction-opening spelling so
/// callers get a typed boundary error; `END` is SQLite's `COMMIT` spelling.
const TRANSACTION_CONTROL_KEYWORDS: [&str; 7] = [
    "BEGIN",
    "START",
    "COMMIT",
    "END",
    "ROLLBACK",
    "SAVEPOINT",
    "RELEASE",
];

/// Skip the same leading whitespace, UTF-8 BOMs, empty statements (`;`), and
/// line/block comments SQLite accepts before an executable statement.
pub(super) fn skip_sqlite_empty_prefix(mut rest: &[u8]) -> &[u8] {
    loop {
        let mut idx = 0;
        while idx < rest.len() && rest[idx].is_ascii_whitespace() {
            idx += 1;
        }
        rest = &rest[idx..];
        if let Some(tail) = rest.strip_prefix(b"\xEF\xBB\xBF") {
            rest = tail;
            continue;
        }
        if let Some(tail) = rest.strip_prefix(b";") {
            rest = tail;
            continue;
        }
        if let Some(tail) = rest.strip_prefix(b"--") {
            let mut idx = 0;
            while idx < tail.len() && tail[idx] != b'\n' {
                idx += 1;
            }
            rest = if idx < tail.len() {
                &tail[idx + 1..]
            } else {
                &[]
            };
            continue;
        }
        if let Some(tail) = rest.strip_prefix(b"/*") {
            let mut idx = 0;
            while idx + 1 < tail.len() && !(tail[idx] == b'*' && tail[idx + 1] == b'/') {
                idx += 1;
            }
            rest = if idx + 1 < tail.len() {
                &tail[idx + 2..]
            } else {
                &[]
            };
            continue;
        }
        break;
    }
    rest
}

/// Return one ASCII SQL token after SQLite whitespace/comments/BOM trivia.
/// Empty-statement separators are deliberately not trivia here: callers use
/// this only after the executable statement head has already been consumed.
pub(super) fn next_sqlite_token(mut rest: &[u8]) -> Option<(&[u8], &[u8])> {
    loop {
        let mut idx = 0;
        while idx < rest.len() && rest[idx].is_ascii_whitespace() {
            idx += 1;
        }
        rest = &rest[idx..];
        if let Some(tail) = rest.strip_prefix(b"\xEF\xBB\xBF") {
            rest = tail;
            continue;
        }
        if let Some(tail) = rest.strip_prefix(b"--") {
            let mut idx = 0;
            while idx < tail.len() && tail[idx] != b'\n' {
                idx += 1;
            }
            rest = if idx < tail.len() {
                &tail[idx + 1..]
            } else {
                &[]
            };
            continue;
        }
        if let Some(tail) = rest.strip_prefix(b"/*") {
            let mut idx = 0;
            while idx + 1 < tail.len() && !(tail[idx] == b'*' && tail[idx + 1] == b'/') {
                idx += 1;
            }
            rest = if idx + 1 < tail.len() {
                &tail[idx + 2..]
            } else {
                &[]
            };
            continue;
        }
        break;
    }

    let len = rest
        .iter()
        .take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_')
        .count();
    (len != 0).then_some((&rest[..len], &rest[len..]))
}

/// Return a transaction-control keyword and the bytes following it, if any.
/// Matching is case-insensitive and requires a word boundary, so an identifier
/// that merely starts with `begin` or `commit` never matches.
fn transaction_control_parts(sql: &str) -> Option<(&'static str, &[u8])> {
    let rest = skip_sqlite_empty_prefix(sql.as_bytes());
    TRANSACTION_CONTROL_KEYWORDS
        .iter()
        .copied()
        .find_map(|keyword| {
            let kw = keyword.as_bytes();
            if rest.len() < kw.len() || !rest[..kw.len()].eq_ignore_ascii_case(kw) {
                return None;
            }
            let boundary = match rest.get(kw.len()) {
                Some(next) => !(next.is_ascii_alphanumeric() || *next == b'_'),
                None => true,
            };
            boundary.then_some((keyword, &rest[kw.len()..]))
        })
}

/// Return the transaction-control keyword heading `sql`, if any.
pub(super) fn transaction_control_head(sql: &str) -> Option<&'static str> {
    transaction_control_parts(sql).map(|(keyword, _)| keyword)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CachedReadTransactionControl {
    /// `BEGIN`, `BEGIN TRANSACTION`, or their explicitly `DEFERRED` form.
    BeginDeferred,
    /// A transaction-ending control statement and its diagnostic keyword.
    Finish(&'static str),
    /// Transaction control that cannot be represented by the single-level
    /// admitted read-transaction state machine.
    Unsupported(&'static str),
}

/// Classify transaction control for a cached read-only connection.
///
/// A cached reader may own exactly one top-level deferred read transaction.
/// Immediate/exclusive starts could reserve write-side locks, while nested
/// savepoint/rollback-to controls would require a second lifecycle level, so
/// both remain rejected. Batch/write paths continue using
/// [`transaction_control_head`] and reject every variant without exception.
pub(super) fn cached_read_transaction_control(sql: &str) -> Option<CachedReadTransactionControl> {
    let (keyword, tail) = transaction_control_parts(sql)?;
    match keyword {
        "BEGIN" => {
            // Accept exactly `BEGIN`, `BEGIN TRANSACTION`, `BEGIN DEFERRED`,
            // or `BEGIN DEFERRED TRANSACTION`, with no trailing tokens.
            // SQLite's grammar also admits `BEGIN TRANSACTION <name>` (the
            // name parses as an identifier and is ignored), so a mode keyword
            // in that trailing position — `BEGIN TRANSACTION IMMEDIATE` —
            // still parses, and classifying it by its first token alone would
            // launder what reads as a write-reserving start into a deferred
            // one. Every trailing token is therefore Unsupported — and the
            // check cannot stop at `next_sqlite_token` returning `None`,
            // because that tokenizer returns `None` for any non-identifier
            // byte, not only end-of-input: a quoted or bracketed tail
            // (`BEGIN TRANSACTION "IMMEDIATE"`, `[IMMEDIATE]`) would fall
            // out of the loop and read as the end of an accepted form. After
            // the accepted keywords, the remainder must reduce to nothing
            // under the same trivia/empty-statement skipping SQLite applies
            // (whitespace, comments, `;`), or the statement is Unsupported.
            let mut rest = tail;
            let mut saw_deferred = false;
            let mut saw_transaction = false;
            while let Some((token, next)) = next_sqlite_token(rest) {
                if !saw_deferred && !saw_transaction && token.eq_ignore_ascii_case(b"DEFERRED") {
                    saw_deferred = true;
                } else if !saw_transaction && token.eq_ignore_ascii_case(b"TRANSACTION") {
                    saw_transaction = true;
                } else {
                    return Some(CachedReadTransactionControl::Unsupported(keyword));
                }
                rest = next;
            }
            if !skip_sqlite_empty_prefix(rest).is_empty() {
                return Some(CachedReadTransactionControl::Unsupported(keyword));
            }
            Some(CachedReadTransactionControl::BeginDeferred)
        }
        "COMMIT" | "END" => Some(CachedReadTransactionControl::Finish(keyword)),
        "ROLLBACK" => {
            let first = next_sqlite_token(tail);
            let rollback_target = match first {
                Some((token, rest)) if token.eq_ignore_ascii_case(b"TRANSACTION") => {
                    next_sqlite_token(rest).map(|(token, _)| token)
                }
                Some((token, _)) => Some(token),
                None => None,
            };
            if rollback_target.is_some_and(|token| token.eq_ignore_ascii_case(b"TO")) {
                Some(CachedReadTransactionControl::Unsupported(keyword))
            } else {
                Some(CachedReadTransactionControl::Finish(keyword))
            }
        }
        _ => Some(CachedReadTransactionControl::Unsupported(keyword)),
    }
}

/// Reject transaction-control statements in `statements` with a typed
/// [`StorageError::InvalidInput`] BEFORE anything executes, preserving the
/// batch's all-or-nothing contract (see [`TRANSACTION_CONTROL_KEYWORDS`]).
pub(super) fn reject_transaction_control_statements(
    statements: &[SqlStatement],
    operation: &'static str,
) -> khive_storage::types::StorageResult<()> {
    for (index, statement) in statements.iter().enumerate() {
        if let Some(keyword) = transaction_control_head(&statement.sql) {
            return Err(StorageError::InvalidInput {
                capability: StorageCapability::Sql,
                operation: operation.into(),
                message: format!(
                    "statement at index {index} is transaction control ({keyword}); \
                     execute_batch owns the BEGIN/COMMIT boundary for the whole \
                     batch — remove transaction-control statements from the batch"
                ),
            });
        }
    }
    Ok(())
}
