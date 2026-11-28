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
#[doc(hidden)]
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

    /// Whether the handler is known to be installed. Once a re-install fails, this returns
    /// false permanently for this connection, even after a later successful re-install,
    /// and every subsequent read scope refuses it.
    pub(crate) fn armed(&self) -> bool {
        !self.unarmed.load(Ordering::Acquire)
    }

    /// Runs `f` with the owned connection, then installs the handler again on return or unwind.
    /// If that install fails the connection stays usable for writes but no read scope accepts it.
    pub fn with_mut<R>(&mut self, f: impl FnOnce(&mut Connection) -> R) -> R {
        struct Rearm<'a> {
            conn: &'a mut Connection,
            id: u64,
            unarmed: &'a AtomicBool,
        }

        impl Drop for Rearm<'_> {
            fn drop(&mut self) {
                if let Err(error) = arm(self.conn, self.id) {
                    self.unarmed.store(true, Ordering::Release);
                    tracing::error!(target: "khive_db::pool", %error, "failed to re-install the read progress handler");
                }
            }
        }

        let rearm = Rearm {
            conn: &mut self.conn,
            id: self.id,
            unarmed: &self.unarmed,
        };
        f(rearm.conn)
    }

    /// Counts progress at `period` VM steps through `hook` until [`Self::test_progress_reset`];
    /// the registered test delegate and read-cancellation verdict still apply.
    #[cfg(test)]
    pub(crate) fn test_progress_every(
        &mut self,
        period: std::os::raw::c_int,
        hook: impl FnMut() -> bool + Send + 'static,
    ) {
        install_progress(&mut self.conn, self.id, period, hook)
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
    install_progress(conn, id, PROGRESS_STEPS, || false)
}

fn install_progress(
    conn: &mut Connection,
    id: u64,
    period: std::os::raw::c_int,
    mut observer: impl FnMut() -> bool + Send + 'static,
) -> rusqlite::Result<()> {
    #[cfg(any(test, feature = "test-support"))]
    let test_slots = crate::test_hooks::slots_of(conn);
    conn.progress_handler(
        period,
        Some(move || {
            if observer() {
                return true;
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read_cancellation::with_traversal_stop;
    use crate::test_hooks::TestHooks;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    const SUM: i64 = 50_005_000;

    fn long_read(conn: &ProgressConn) -> rusqlite::Result<i64> {
        conn.query_row(
            "WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL \
             SELECT x + 1 FROM n WHERE x < 10000) SELECT sum(x) FROM n",
            [],
            |row| row.get(0),
        )
    }

    fn assert_interrupted(result: rusqlite::Result<i64>, phase: &str) {
        assert!(
            matches!(&result, Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::OperationInterrupted),
            "{phase}: SQLite must interrupt the query, got {result:?}"
        );
    }

    #[test]
    fn with_mut_rearms_after_return_and_unwind() {
        for panics in [false, true] {
            let mut conn = ProgressConn::new(Connection::open_in_memory().unwrap()).unwrap();
            assert_eq!(long_read(&conn).unwrap(), SUM);
            let result = catch_unwind(AssertUnwindSafe(|| {
                conn.with_mut(|raw| {
                    raw.progress_handler(0, None::<fn() -> bool>).unwrap();
                    if panics {
                        std::panic::panic_any(73_u64);
                    }
                    42
                })
            }));
            if panics {
                assert_eq!(result.unwrap_err().downcast_ref::<u64>(), Some(&73));
            } else {
                assert_eq!(result.unwrap(), 42);
            }
            assert!(conn.armed());
            assert_interrupted(
                with_traversal_stop(&conn, || true, || long_read(&conn)),
                if panics {
                    "after unwind"
                } else {
                    "after return"
                },
            );
            assert_eq!(long_read(&conn).unwrap(), SUM);
        }
    }

    #[test]
    fn test_progress_period_preserves_delegate_and_stop() {
        let mut conn = ProgressConn::new(Connection::open_in_memory().unwrap()).unwrap();
        let observed = Arc::new(AtomicUsize::new(0));
        let observer = Arc::clone(&observed);
        conn.test_progress_every(1, move || {
            observer.fetch_add(1, Ordering::Relaxed);
            false
        });
        let delegated = Arc::new(AtomicUsize::new(0));
        let delegate = Arc::clone(&delegated);
        conn.test_progress_handler(Some(move || {
            delegate.fetch_add(1, Ordering::Relaxed);
            true
        }))
        .unwrap();
        assert_interrupted(long_read(&conn), "registered delegate with test cadence");
        assert!(observed.load(Ordering::Relaxed) > 0);
        assert!(delegated.load(Ordering::Relaxed) > 0);

        conn.test_progress_handler(None::<fn() -> bool>).unwrap();
        let delegated_before = delegated.load(Ordering::Relaxed);
        assert_eq!(long_read(&conn).unwrap(), SUM);
        assert_eq!(delegated.load(Ordering::Relaxed), delegated_before);
        assert_interrupted(
            with_traversal_stop(&conn, || true, || long_read(&conn)),
            "real stop with test cadence and no delegate",
        );
        assert_eq!(long_read(&conn).unwrap(), SUM);

        let delegate = Arc::clone(&delegated);
        conn.test_progress_handler(Some(move || {
            delegate.fetch_add(1, Ordering::Relaxed);
            true
        }))
        .unwrap();
        let observed_before = observed.load(Ordering::Relaxed);
        conn.test_progress_reset();
        assert_interrupted(long_read(&conn), "registered delegate after reset");
        assert_eq!(observed.load(Ordering::Relaxed), observed_before);
        assert!(delegated.load(Ordering::Relaxed) > delegated_before);

        let observer = Arc::clone(&observed);
        conn.test_progress_every(1, move || {
            observer.fetch_add(1, Ordering::Relaxed);
            true
        });
        let delegated_before = delegated.load(Ordering::Relaxed);
        assert_interrupted(long_read(&conn), "test observer stops before the delegate");
        assert!(observed.load(Ordering::Relaxed) > observed_before);
        assert_eq!(delegated.load(Ordering::Relaxed), delegated_before);
        conn.test_progress_reset();
        conn.test_progress_handler(None::<fn() -> bool>).unwrap();
        assert_eq!(long_read(&conn).unwrap(), SUM);
    }
}
