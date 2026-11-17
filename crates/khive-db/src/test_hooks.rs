//! Test-only authorizer and progress hooks reachable through a shared connection borrow.
//!
//! rusqlite 0.41 takes `&mut Connection` to install any hook, but fault-injection tests run
//! inside write callbacks and pool leases that only lend `&Connection`. A connection opened by
//! the pool or the writer task is registered here at open with a delegating authorizer; a test
//! swaps the delegate through [`TestHooks`]. Swapping flushes the statement cache, as installing
//! an authorizer expires prepared statements. A [`crate::ProgressConn`] also consults the
//! connection's test progress delegate before the read-cancellation verdict.
//!
//! Keys are SQLite handle addresses. Registration replaces any stale entry at the same address,
//! and a [`crate::ProgressConn`] removes its entry when it gives up the connection.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Weak};

use parking_lot::Mutex;
use rusqlite::hooks::{Action, AuthContext, Authorization};
use rusqlite::Connection;

type AuthorizerDelegate = Box<dyn for<'r> FnMut(AuthContext<'r>) -> Authorization + Send>;
type ProgressDelegate = Box<dyn FnMut() -> bool + Send>;
type UpdateDelegate = Box<dyn FnMut(Action, &str, &str, i64) + Send>;

#[derive(Default)]
pub(crate) struct Slots {
    authorizer: Mutex<Option<AuthorizerDelegate>>,
    progress: Mutex<Option<ProgressDelegate>>,
    update: Mutex<Option<UpdateDelegate>>,
    suppression: Mutex<Option<SuppressionArm>>,
    active_reply: Mutex<Weak<CommitReplyRequest>>,
    // Set while `test_authorizer` checks that the delegating authorizer is still installed.
    probing: AtomicBool,
    probed: AtomicBool,
}

/// Observations from one matched writer request; no serialized error is injected.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommitReplySuppressionSnapshot {
    pub matched_requests: usize,
    pub commit_issued: bool,
    pub commit_succeeded: bool,
    pub autocommit: bool,
    pub reply_suppressed: usize,
}

#[derive(Clone, Default)]
pub struct CommitReplySuppressionProbe(Arc<Mutex<CommitReplySuppressionSnapshot>>);

impl CommitReplySuppressionProbe {
    pub fn snapshot(&self) -> CommitReplySuppressionSnapshot {
        *self.0.lock()
    }
}

struct SuppressionArm {
    table: String,
    rowid: i64,
    probe: CommitReplySuppressionProbe,
}

#[derive(Default)]
struct CommitReplyRequest {
    probe: Mutex<Option<CommitReplySuppressionProbe>>,
}

pub(crate) struct CommitReplyRequestGuard {
    slots: Arc<Slots>,
    request: Arc<CommitReplyRequest>,
}

impl CommitReplyRequestGuard {
    pub(crate) fn suppress_successful_reply(&self) -> bool {
        let probe = self.request.probe.lock();
        let Some(probe) = probe.as_ref() else {
            return false;
        };
        let mut snapshot = probe.0.lock();
        if snapshot.commit_issued && snapshot.commit_succeeded && snapshot.autocommit {
            snapshot.reply_suppressed += 1;
            true
        } else {
            false
        }
    }
}

impl Drop for CommitReplyRequestGuard {
    fn drop(&mut self) {
        let mut active = self.slots.active_reply.lock();
        if active.ptr_eq(&Arc::downgrade(&self.request)) {
            *active = Weak::new();
        }
    }
}

pub(crate) fn begin_commit_reply_request(conn: &Connection) -> Option<CommitReplyRequestGuard> {
    let slots = slots_of(conn)?;
    let request = Arc::new(CommitReplyRequest::default());
    *slots.active_reply.lock() = Arc::downgrade(&request);
    Some(CommitReplyRequestGuard { slots, request })
}

fn active_commit_probe(conn: &Connection) -> Option<CommitReplySuppressionProbe> {
    let slots = slots_of(conn)?;
    let request = slots.active_reply.lock().upgrade()?;
    let probe = request.probe.lock().clone();
    probe
}

pub(crate) fn commit_reply_issued(conn: &Connection) {
    if let Some(probe) = active_commit_probe(conn) {
        probe.0.lock().commit_issued = true;
    }
}

pub(crate) fn commit_reply_completed(conn: &Connection, succeeded: bool, autocommit: bool) {
    if let Some(probe) = active_commit_probe(conn) {
        let mut snapshot = probe.0.lock();
        snapshot.commit_succeeded = succeeded;
        snapshot.autocommit = autocommit;
    }
}

fn match_commit_reply(slots: &Slots, action: Action, database: &str, table: &str, rowid: i64) {
    if !matches!(action, Action::SQLITE_INSERT) || database != "main" {
        return;
    }
    let Some(request) = slots.active_reply.lock().upgrade() else {
        return;
    };
    let mut arm = slots.suppression.lock();
    if arm
        .as_ref()
        .is_some_and(|arm| arm.table == table && arm.rowid == rowid)
    {
        let arm = arm.take().expect("matched suppression arm");
        arm.probe.0.lock().matched_requests += 1;
        *request.probe.lock() = Some(arm.probe);
    }
}

// Weak, so a connection's delegates are dropped with the connection's own hook closures.
static SLOTS: LazyLock<Mutex<HashMap<usize, Weak<Slots>>>> = LazyLock::new(Default::default);

fn key(conn: &Connection) -> usize {
    // SAFETY: the handle is read only as an address to key the registry; it is never
    // dereferenced or passed to SQLite.
    unsafe { conn.handle() as usize }
}

fn slots(conn: &Connection) -> Arc<Slots> {
    slots_of(conn).expect("connection was not opened through the pool or writer task test registry")
}

/// `conn`'s live registration, if it has one.
pub(crate) fn slots_of(conn: &Connection) -> Option<Arc<Slots>> {
    SLOTS.lock().get(&key(conn)).and_then(Weak::upgrade)
}

/// Registers a freshly opened `conn`, replacing any stale entry at its address, and installs
/// its delegating authorizer.
pub(crate) fn register(conn: &mut Connection) -> rusqlite::Result<()> {
    let slots = Arc::new(Slots::default());
    SLOTS.lock().insert(key(conn), Arc::downgrade(&slots));
    install(conn, slots)
}

/// Keeps `conn`'s registration untouched when it has one, so a hook a test installed before
/// handing the connection on survives, and registers it otherwise.
pub(crate) fn adopt(conn: &mut Connection) -> rusqlite::Result<()> {
    if slots_of(conn).is_some() {
        return Ok(());
    }
    register(conn)
}

fn install(conn: &mut Connection, slots: Arc<Slots>) -> rusqlite::Result<()> {
    // A connection that is not a `ProgressConn` (the writer task's, a raw fixture's) gets the
    // test progress delegate alone; `ProgressConn::new` replaces this with its own handler, which
    // consults the delegate first.
    let update_slots = Arc::clone(&slots);
    conn.update_hook(Some(
        move |action: Action, database: &str, table: &str, rowid: i64| {
            match_commit_reply(&update_slots, action, database, table, rowid);
            if let Some(delegate) = update_slots.update.lock().as_mut() {
                delegate(action, database, table, rowid);
            }
        },
    ))?;
    let progress_slots = Arc::clone(&slots);
    conn.progress_handler(1_000, Some(move || progress(&progress_slots)))?;
    conn.authorizer(Some(move |context: AuthContext<'_>| {
        if slots.probing.load(Ordering::Acquire) {
            slots.probed.store(true, Ordering::Release);
            return Authorization::Allow;
        }
        match slots.authorizer.lock().as_mut() {
            Some(delegate) => delegate(context),
            None => Authorization::Allow,
        }
    }))
}

/// `conn`, registered.
pub fn registered(mut conn: Connection) -> Connection {
    register(&mut conn).expect("install the test authorizer delegate");
    conn
}

/// Registers `conn` when hooks are compiled in; a no-op otherwise.
#[doc(hidden)]
pub(crate) fn register_opened(conn: Connection) -> rusqlite::Result<Connection> {
    let mut conn = conn;
    register(&mut conn)?;
    Ok(conn)
}

/// Removes `conn`'s registration.
pub(crate) fn unregister(conn: &Connection) {
    SLOTS.lock().remove(&key(conn));
}

/// The test progress delegate's verdict, if one is set.
pub(crate) fn progress(slots: &Slots) -> bool {
    slots
        .progress
        .lock()
        .as_mut()
        .is_some_and(|delegate| delegate())
}

/// Hook installation for tests, through a shared borrow of a registered connection.
pub trait TestHooks {
    /// Suppress only the successful COMMIT reply of the request that inserts this
    /// exact row in `main`. Other requests and failed settlements keep their replies.
    fn test_suppress_commit_reply_after_update(
        &self,
        table: &str,
        rowid: i64,
    ) -> CommitReplySuppressionProbe;

    /// Replaces the test authorizer delegate (`None` removes it). Setting one fails when a raw
    /// `Connection::authorizer` call has replaced the delegating authorizer, which would never
    /// consult it.
    fn test_authorizer<F>(&self, hook: Option<F>) -> rusqlite::Result<()>
    where
        F: for<'r> FnMut(AuthContext<'r>) -> Authorization + Send + 'static;

    /// Replaces the test progress delegate (`None` removes it). It runs at each progress
    /// callback of a pooled connection, before the read-cancellation verdict.
    fn test_progress_handler<F>(&self, hook: Option<F>) -> rusqlite::Result<()>
    where
        F: FnMut() -> bool + Send + 'static;

    /// Replaces the test update-hook delegate (`None` removes it).
    fn test_update_hook<F>(&self, hook: Option<F>) -> rusqlite::Result<()>
    where
        F: FnMut(Action, &str, &str, i64) + Send + 'static;
}

impl TestHooks for Connection {
    fn test_suppress_commit_reply_after_update(
        &self,
        table: &str,
        rowid: i64,
    ) -> CommitReplySuppressionProbe {
        let probe = CommitReplySuppressionProbe::default();
        *slots(self).suppression.lock() = Some(SuppressionArm {
            table: table.into(),
            rowid,
            probe: probe.clone(),
        });
        probe
    }

    fn test_authorizer<F>(&self, hook: Option<F>) -> rusqlite::Result<()>
    where
        F: for<'r> FnMut(AuthContext<'r>) -> Authorization + Send + 'static,
    {
        let slots = slots(self);
        let setting = hook.is_some();
        *slots.authorizer.lock() = hook.map(|hook| Box::new(hook) as AuthorizerDelegate);
        self.flush_prepared_statement_cache();
        if setting {
            slots.probed.store(false, Ordering::Release);
            slots.probing.store(true, Ordering::Release);
            let prepared = self.prepare("SELECT 1").map(drop);
            slots.probing.store(false, Ordering::Release);
            prepared?;
            if !slots.probed.load(Ordering::Acquire) {
                *slots.authorizer.lock() = None;
                return Err(rusqlite::Error::UserFunctionError(
                    "test_authorizer: a raw authorizer replaced the delegating one on this \
                     connection, so the delegate would never run"
                        .into(),
                ));
            }
        }
        Ok(())
    }

    fn test_progress_handler<F>(&self, hook: Option<F>) -> rusqlite::Result<()>
    where
        F: FnMut() -> bool + Send + 'static,
    {
        *slots(self).progress.lock() = hook.map(|hook| Box::new(hook) as ProgressDelegate);
        Ok(())
    }

    fn test_update_hook<F>(&self, hook: Option<F>) -> rusqlite::Result<()>
    where
        F: FnMut(Action, &str, &str, i64) + Send + 'static,
    {
        *slots(self).update.lock() = hook.map(|hook| Box::new(hook) as UpdateDelegate);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_test_authorizer_delegate_runs_on_a_registered_connection() {
        let conn = registered(Connection::open_in_memory().unwrap());
        conn.test_authorizer(Some(|context: AuthContext<'_>| match context.action {
            rusqlite::hooks::AuthAction::Select => Authorization::Deny,
            _ => Authorization::Allow,
        }))
        .unwrap();
        assert!(
            conn.prepare("SELECT 1").is_err(),
            "the delegate denies SELECT"
        );
        conn.test_authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
            .unwrap();
        assert!(conn.prepare("SELECT 1").is_ok());
    }

    #[test]
    fn a_test_authorizer_refuses_after_a_raw_authorizer_replaced_the_delegate() {
        let mut conn = registered(Connection::open_in_memory().unwrap());
        conn.authorizer(Some(|_: AuthContext<'_>| Authorization::Allow))
            .unwrap();
        let refused = conn.test_authorizer(Some(|_: AuthContext<'_>| Authorization::Deny));
        assert!(refused.is_err(), "the delegate would never run");
        assert!(slots(&conn).authorizer.lock().is_none());
    }
}
