//! A SQLite connection whose progress handler is installed once, when it is built.
//!
//! rusqlite 0.41 takes `&mut Connection` to install any hook, while read scopes run on shared
//! borrows handed out by the pool. So the handler is installed here, with the owned connection,
//! and never per read: it asks the per-thread read registration whether a statement on THIS
//! connection should stop (`read_cancellation::progress_stop`). A read scope accepts only a
//! `ProgressConn`, so a connection without the handler cannot reach one.

use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rusqlite::Connection;

/// SQLite VM steps between progress callbacks.
const PROGRESS_STEPS: std::os::raw::c_int = 1_000;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// A connection carrying the read-cancellation progress handler and a process-unique identity.
///
/// It dereferences to [`Connection`] for statements but gives no `&mut Connection` outside
/// [`ProgressConn::with_mut`], which re-installs the handler afterwards, so nothing can replace
/// or remove it:
/// ```compile_fail
/// use khive_db::ProgressConn;
/// let mut conn = ProgressConn::new(rusqlite::Connection::open_in_memory().unwrap()).unwrap();
/// conn.progress_handler(0, None::<fn() -> bool>).unwrap();
/// ```
#[derive(Debug)]
pub struct ProgressConn {
    conn: Connection,
    id: u64,
    unarmed: AtomicBool,
}

impl ProgressConn {
    /// Takes the connection and installs the handler.
    pub fn new(mut conn: Connection) -> rusqlite::Result<Self> {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        #[cfg(any(test, feature = "test-support"))]
        crate::test_hooks::adopt(&mut conn)?;
        arm(&mut conn, id)?;
        Ok(ProgressConn {
            conn,
            id,
            unarmed: AtomicBool::new(false),
        })
    }

    /// The identity a read registration names.
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// Whether the handler is known to be installed. A failed re-install leaves the connection
    /// unarmed, and a read scope then refuses it.
    pub(crate) fn armed(&self) -> bool {
        !self.unarmed.load(Ordering::Acquire)
    }

    /// Runs `f` with the owned connection, then installs the handler again whatever `f` did. If
    /// that install fails the connection stays usable for writes but no read scope accepts it.
    pub fn with_mut<R>(&mut self, f: impl FnOnce(&mut Connection) -> R) -> R {
        let result = f(&mut self.conn);
        if let Err(error) = arm(&mut self.conn, self.id) {
            self.unarmed.store(true, Ordering::Release);
            tracing::error!(target: "khive_db::pool", %error, "failed to re-install the read progress handler");
        }
        result
    }

    /// Counts progress at `period` VM steps through `hook` until [`Self::test_progress_reset`];
    /// the read-cancellation verdict still applies.
    #[cfg(test)]
    pub(crate) fn test_progress_every(
        &mut self,
        period: std::os::raw::c_int,
        mut hook: impl FnMut() -> bool + Send + 'static,
    ) {
        let id = self.id;
        self.conn
            .progress_handler(
                period,
                Some(move || hook() || crate::read_cancellation::progress_stop(id)),
            )
            .expect("install the test progress handler");
    }

    #[cfg(test)]
    pub(crate) fn test_progress_reset(&mut self) {
        arm(&mut self.conn, self.id).expect("re-install the progress handler");
    }

    /// The connection, for closing it. The handler stays installed and its identity is never
    /// registered again, so it never stops a statement.
    pub(crate) fn into_inner(self) -> Connection {
        #[cfg(any(test, feature = "test-support"))]
        crate::test_hooks::unregister(&self.conn);
        self.conn
    }
}

impl Deref for ProgressConn {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        &self.conn
    }
}

fn arm(conn: &mut Connection, id: u64) -> rusqlite::Result<()> {
    #[cfg(any(test, feature = "test-support"))]
    let test_slots = crate::test_hooks::slots_of(conn);
    conn.progress_handler(
        PROGRESS_STEPS,
        Some(move || {
            #[cfg(any(test, feature = "test-support"))]
            if test_slots
                .as_deref()
                .is_some_and(crate::test_hooks::progress)
            {
                return true;
            }
            crate::read_cancellation::progress_stop(id)
        }),
    )
}
