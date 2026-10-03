//! SQLite statement-start observations for isolated test pools.
//!
//! A started statement is an execution attempt, including a step that later
//! fails. Preparation alone, returned rows, successful completion and affected
//! rows are different observations. This module records unexpanded SQL only;
//! bound values are never read. It is absent without `test-support` or tests.

use std::collections::HashMap;
use std::ffi::{c_int, c_uint, c_void, CStr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use parking_lot::Mutex;
use rusqlite::{ffi, Connection};

use crate::SqliteError;

/// One top-level SQLite statement that began running.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartedStatement {
    /// Original, unexpanded SQL text, including any parameter placeholders.
    pub sql: String,
    /// SQLite's classification of the prepared statement.
    pub readonly: bool,
}

#[derive(Default)]
struct Records {
    statements: Vec<StartedStatement>,
    lost: bool,
}

struct Probe {
    limit: usize,
    records: Mutex<Records>,
}

static NEXT_HUB: AtomicUsize = AtomicUsize::new(1);
static HUBS: OnceLock<Mutex<HashMap<usize, Weak<StatementObserverHub>>>> = OnceLock::new();

fn hubs() -> &'static Mutex<HashMap<usize, Weak<StatementObserverHub>>> {
    HUBS.get_or_init(Mutex::default)
}

/// One pool's trace slot, retained by the pool and active observation guards.
pub(crate) struct StatementObserverHub {
    id: usize,
    active: Mutex<Option<Arc<Probe>>>,
}

/// Scoped observation of all connections owned by one private test pool.
///
/// This includes its queued writer and replacement/standalone connections.
/// Each connection runs its setup statements before observation starts on that
/// connection; those statements are never recorded, even when the connection
/// opens during an active observation. Reader connection opens are tracked by the
/// existing reader acquisition counters, separately from statement starts.
/// Unrelated work on the same pool is included; use an isolated pool without
/// background work and select the target SQL from the resulting records.
/// Only one observation may be active per pool. Dropping this guard stops new
/// observations, including during unwinding. It does not cancel database work.
pub struct StatementStartObservation {
    hub: Arc<StatementObserverHub>,
    probe: Arc<Probe>,
}

impl StatementObserverHub {
    pub(crate) fn new() -> Result<Arc<Self>, SqliteError> {
        let id = NEXT_HUB
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| {
                SqliteError::InvalidData("statement observer identity exhausted".into())
            })?;
        let hub = Arc::new(Self {
            id,
            active: Mutex::new(None),
        });
        hubs().lock().insert(id, Arc::downgrade(&hub));
        Ok(hub)
    }

    pub(crate) fn observe(
        self: &Arc<Self>,
        limit: usize,
    ) -> Result<StatementStartObservation, SqliteError> {
        if limit == 0 {
            return Err(SqliteError::InvalidData(
                "statement observation requires a nonzero record limit".into(),
            ));
        }
        let mut active = self.active.lock();
        if active.is_some() {
            return Err(SqliteError::InvalidData(
                "a statement observation is already active for this pool".into(),
            ));
        }
        let probe = Arc::new(Probe {
            limit,
            records: Mutex::new(Records::default()),
        });
        *active = Some(Arc::clone(&probe));
        Ok(StatementStartObservation {
            hub: Arc::clone(self),
            probe,
        })
    }
}

impl Drop for StatementObserverHub {
    fn drop(&mut self) {
        hubs().lock().remove(&self.id);
    }
}

impl StatementStartObservation {
    /// Copy the observed statement starts in callback arrival order.
    ///
    /// An exhausted record budget or failed callback is an error, never a
    /// truncated count. This order does not promise inter-connection causality.
    pub fn started_statements(&self) -> Result<Vec<StartedStatement>, SqliteError> {
        let records = self.probe.records.lock();
        if records.lost {
            return Err(SqliteError::InvalidData(
                "statement observation lost records; its count is incomplete".into(),
            ));
        }
        Ok(records.statements.clone())
    }
}

impl Drop for StatementStartObservation {
    fn drop(&mut self) {
        let mut active = self.hub.active.lock();
        if active
            .as_ref()
            .is_some_and(|probe| Arc::ptr_eq(probe, &self.probe))
        {
            *active = None;
        }
    }
}

/// Install once on a newly opened connection, before it is shared.
///
/// SQLite keeps one trace callback per connection, and this test-only
/// instrumentation owns it. Test code that needs statement text must go
/// through `ConnectionPool::observe_test_statement_starts` instead of
/// installing its own `sqlite3_trace_v2` hook: replacing or clearing the slot
/// silently stops this observer, and its records then read as an empty success.
pub(crate) fn install(
    conn: &Connection,
    hub: &Arc<StatementObserverHub>,
) -> Result<(), SqliteError> {
    // SAFETY: this newly opened connection is exclusively borrowed. SQLite
    // stores an opaque, non-dereferenced monotonic token. The callback upgrades
    // its weak registry entry; a connection may safely outlive its pool.
    let result = unsafe {
        ffi::sqlite3_trace_v2(
            conn.handle(),
            ffi::SQLITE_TRACE_STMT as c_uint,
            Some(trace),
            hub.id as *mut c_void,
        )
    };
    if result != ffi::SQLITE_OK {
        return Err(rusqlite::Error::SqliteFailure(ffi::Error::new(result), None).into());
    }
    Ok(())
}

unsafe extern "C" fn trace(
    event: c_uint,
    context: *mut c_void,
    statement: *mut c_void,
    text: *mut c_void,
) -> c_int {
    if event != ffi::SQLITE_TRACE_STMT as c_uint {
        return 0;
    }
    let hub = {
        let registry = hubs().lock();
        registry.get(&(context as usize)).and_then(Weak::upgrade)
    };
    let Some(hub) = hub else { return 0 };
    let active = hub.active.lock();
    let Some(probe) = active.as_ref() else {
        return 0;
    };
    let mut records = probe.records.lock();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let stmt = statement.cast::<ffi::sqlite3_stmt>();
        // SAFETY: the prepared statement remains live for this callback.
        let sql = unsafe { ffi::sqlite3_sql(stmt) };
        if sql.is_null() || text.is_null() {
            records.lost = true;
            return;
        }
        // SQLITE_TRACE_STMT also reports trigger subprogram comments. Only
        // the event containing the statement's original text is top-level.
        // Comparing full bytes also admits ordinary SQL beginning with "--".
        let original = unsafe { CStr::from_ptr(sql) };
        let reported = unsafe { CStr::from_ptr(text.cast()) };
        if original.to_bytes() != reported.to_bytes() {
            return;
        }
        if records.statements.len() == probe.limit {
            records.lost = true;
            return;
        }
        records.statements.push(StartedStatement {
            sql: original.to_string_lossy().into_owned(),
            readonly: unsafe { ffi::sqlite3_stmt_readonly(stmt) != 0 },
        });
    }));
    if result.is_err() {
        records.lost = true;
    }
    0
}

#[cfg(test)]
mod tests;
