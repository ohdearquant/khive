//! Connection pool for SQLite: one exclusive writer, N concurrent readers.
use crossbeam_queue::ArrayQueue;
use parking_lot::Mutex;
use rusqlite::{Connection, OpenFlags};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::error::SqliteError;

const CACHE_SIZE_KIB: &str = "-65536";
const MMAP_SIZE_BYTES: &str = "1073741824";
const DEFAULT_READER_CAP: usize = 8;

const DEFAULT_WAL_AUTOCHECKPOINT_PAGES: u32 = 4000;
const DEFAULT_JOURNAL_SIZE_LIMIT_BYTES: i64 = 67_108_864; // 64 MiB

/// Configuration for the connection pool.
#[derive(Clone, Debug)]
pub struct PoolConfig {
    /// Database path. None = in-memory (pool degrades to single connection).
    pub path: Option<PathBuf>,
    /// Number of reader connections (default: min(num_cpus, 8)).
    pub max_readers: usize,
    /// WAL mode (must be true for pooling to work; default: true).
    pub wal_mode: bool,
    /// Busy timeout per connection (default: 30s).
    ///
    /// Overridable via `KHIVE_BUSY_TIMEOUT_SECS`.
    pub busy_timeout: Duration,
    /// Time to wait for a reader connection before returning an error (default: 5s).
    ///
    /// Overridable via `KHIVE_CHECKOUT_TIMEOUT_SECS`.
    pub checkout_timeout: Duration,
    /// Number of WAL pages that triggers an automatic checkpoint.
    ///
    /// Maps to `PRAGMA wal_autocheckpoint`. The default (4000 pages, ~16 MiB
    /// at SQLite's default 4 KiB page size) matches the pre-config behaviour.
    ///
    /// Overridable via `KHIVE_WAL_AUTOCHECKPOINT_PAGES`.
    pub wal_autocheckpoint_pages: u32,
    /// Maximum WAL journal size in bytes before SQLite resets the WAL.
    ///
    /// Maps to `PRAGMA journal_size_limit`. Default: 64 MiB.
    ///
    /// Overridable via `KHIVE_JOURNAL_SIZE_LIMIT_BYTES`.
    pub journal_size_limit_bytes: i64,
    /// Open the database read-only (default: false).
    ///
    /// When true, the pool's writer connection is opened with
    /// `SQLITE_OPEN_READ_ONLY` (no `SQLITE_OPEN_CREATE`, so a missing path is
    /// rejected instead of created) and `PRAGMA query_only = ON` is set on
    /// every connection that can execute SQL. Reader connections are already
    /// opened read-only regardless of this flag.
    pub read_only: bool,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            path: None,
            max_readers: std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
                .clamp(1, DEFAULT_READER_CAP),
            wal_mode: true,
            busy_timeout: Duration::from_secs(
                std::env::var("KHIVE_BUSY_TIMEOUT_SECS")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(30),
            ),
            checkout_timeout: Duration::from_secs(
                std::env::var("KHIVE_CHECKOUT_TIMEOUT_SECS")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(5),
            ),
            wal_autocheckpoint_pages: std::env::var("KHIVE_WAL_AUTOCHECKPOINT_PAGES")
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(DEFAULT_WAL_AUTOCHECKPOINT_PAGES),
            journal_size_limit_bytes: std::env::var("KHIVE_JOURNAL_SIZE_LIMIT_BYTES")
                .ok()
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(DEFAULT_JOURNAL_SIZE_LIMIT_BYTES),
            read_only: false,
        }
    }
}

/// A read-write connection pool for SQLite.
///
/// Architecture:
/// - 1 writer connection protected by a Mutex (exclusive access)
/// - N reader connections in a lock-free queue (concurrent access)
/// - All connections share the same database file in WAL mode
///
/// For in-memory databases, or when WAL mode is disabled/unavailable, the pool
/// degrades to single-connection mode and routes all operations through the
/// writer connection.
pub struct ConnectionPool {
    writer: Arc<Mutex<Connection>>,
    readers: ArrayQueue<Connection>,
    max_readers: usize,
    config: PoolConfig,
}

enum ReaderLease<'pool> {
    Pooled(Connection),
    Shared(parking_lot::MutexGuard<'pool, Connection>),
}

/// A reader connection checked out from the pool.
/// Returns the connection to the pool on drop.
pub struct ReaderGuard<'pool> {
    lease: Option<ReaderLease<'pool>>,
    pool: &'pool ConnectionPool,
}

impl<'pool> ReaderGuard<'pool> {
    /// Access the connection.
    pub fn conn(&self) -> &Connection {
        match self
            .lease
            .as_ref()
            .expect("reader guard missing connection")
        {
            ReaderLease::Pooled(conn) => conn,
            ReaderLease::Shared(guard) => guard,
        }
    }
}

impl<'pool> Deref for ReaderGuard<'pool> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.conn()
    }
}

impl<'pool> Drop for ReaderGuard<'pool> {
    fn drop(&mut self) {
        let Some(lease) = self.lease.take() else {
            return;
        };

        match lease {
            ReaderLease::Pooled(conn) => self.pool.return_reader(conn),
            ReaderLease::Shared(_guard) => {}
        }
    }
}

/// A writer connection checked out from the pool.
/// The Mutex ensures only one writer at a time.
pub struct WriterGuard<'pool> {
    guard: parking_lot::MutexGuard<'pool, Connection>,
}

impl<'pool> WriterGuard<'pool> {
    /// Returns a shared reference to the underlying connection.
    pub fn conn(&self) -> &Connection {
        &self.guard
    }

    /// Returns a mutable reference to the underlying connection.
    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.guard
    }

    /// Execute a write transaction.
    /// Wraps the closure in BEGIN IMMEDIATE ... COMMIT.
    pub fn transaction<F, R>(&self, f: F) -> Result<R, SqliteError>
    where
        F: FnOnce(&Connection) -> Result<R, SqliteError>,
    {
        self.guard.execute_batch("BEGIN IMMEDIATE")?;
        let _tx_handle = khive_storage::tx_registry::register(Some("writer_guard_tx".to_string()));

        match f(&self.guard) {
            Ok(result) => {
                if let Err(err) = self.guard.execute_batch("COMMIT") {
                    let _ = self.guard.execute_batch("ROLLBACK");
                    return Err(err.into());
                }
                Ok(result)
            }
            Err(err) => {
                let _ = self.guard.execute_batch("ROLLBACK");
                Err(err)
            }
        }
    }
}

impl<'pool> Deref for WriterGuard<'pool> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.conn()
    }
}

impl<'pool> DerefMut for WriterGuard<'pool> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.conn_mut()
    }
}

impl ConnectionPool {
    /// Create a new connection pool.
    ///
    /// Opens 1 writer + N reader connections to the same database when pooling
    /// is enabled. All connections are configured consistently (busy timeout,
    /// foreign keys, cache, mmap, temp store). For in-memory databases, or when
    /// WAL is disabled or unavailable, the pool falls back to single-connection
    /// mode.
    pub fn new(config: PoolConfig) -> Result<Self, SqliteError> {
        let writer = open_writer_connection(&config)?;
        let wal_enabled = configure_writer_connection(&writer, &config)?;
        let max_readers = effective_reader_count(&config, wal_enabled);

        let readers = ArrayQueue::new(max_readers.max(1));

        let pool = Self {
            writer: Arc::new(Mutex::new(writer)),
            readers,
            max_readers,
            config,
        };

        for _ in 0..pool.max_readers {
            let conn = pool.open_reader_connection()?;
            pool.readers
                .push(conn)
                .expect("reader queue must have capacity during pool initialization");
        }

        Ok(pool)
    }

    /// Check out a reader connection.
    ///
    /// Tries to pop from the lock-free queue. If empty, spins briefly then
    /// waits with exponential backoff up to `checkout_timeout`.
    ///
    /// # Deadlock Warning
    ///
    /// In degraded mode (WAL unavailable, `max_readers == 0`), this method locks
    /// the writer mutex. If the calling thread already holds a [`WriterGuard`],
    /// this will deadlock (parking_lot `Mutex` is not reentrant). Never call
    /// `reader()` while holding a `WriterGuard` on the same pool.
    pub fn reader(&self) -> Result<ReaderGuard<'_>, SqliteError> {
        if self.max_readers == 0 {
            return Ok(ReaderGuard {
                lease: Some(ReaderLease::Shared(self.writer.lock())),
                pool: self,
            });
        }

        let started = Instant::now();
        let mut attempt = 0u32;

        loop {
            if let Some(conn) = self.readers.pop() {
                return Ok(ReaderGuard {
                    lease: Some(ReaderLease::Pooled(conn)),
                    pool: self,
                });
            }

            if started.elapsed() >= self.config.checkout_timeout {
                return Err(pool_exhausted_error(
                    self.config.checkout_timeout,
                    self.max_readers,
                ));
            }

            match attempt {
                0..=7 => {
                    let spins = 1usize << attempt;
                    for _ in 0..spins {
                        std::hint::spin_loop();
                    }
                }
                8..=15 => thread::yield_now(),
                _ => {
                    let remaining = self
                        .config
                        .checkout_timeout
                        .saturating_sub(started.elapsed());
                    let sleep = Duration::from_micros(50 * (1u64 << (attempt - 16).min(6)));
                    thread::sleep(sleep.min(remaining).min(Duration::from_millis(2)));
                }
            }

            attempt = attempt.saturating_add(1);
        }
    }

    /// Check out the writer connection.
    ///
    /// Waits up to `checkout_timeout` for the writer Mutex and returns
    /// `Err(SqliteError::InvalidData)` if the timeout is exceeded.
    pub fn writer(&self) -> Result<WriterGuard<'_>, SqliteError> {
        let guard = self
            .writer
            .try_lock_for(self.config.checkout_timeout)
            .ok_or_else(|| {
                SqliteError::InvalidData(format!(
                    "timed out after {:?} waiting for sqlite writer connection",
                    self.config.checkout_timeout
                ))
            })?;
        Ok(WriterGuard { guard })
    }

    /// Non-panicking writer checkout.
    ///
    /// Returns `Err` on timeout instead of panicking. Use this in request
    /// handlers where a 500 is preferable to crashing the process.
    pub fn try_writer(&self) -> Result<WriterGuard<'_>, SqliteError> {
        self.writer()
    }

    /// Zero-wait writer checkout for background tasks.
    ///
    /// Uses `try_lock()` (no timeout, no spin) — returns `Err` immediately when
    /// any other caller holds the writer Mutex. Background tasks (e.g. the WAL
    /// checkpoint task) MUST use this instead of `try_writer` so that a busy
    /// writer causes the background task to skip its current tick rather than
    /// stalling for up to `checkout_timeout` (default 5s) while write traffic
    /// is in progress.
    pub fn try_writer_nowait(&self) -> Result<WriterGuard<'_>, SqliteError> {
        let guard = self.writer.try_lock().ok_or_else(|| {
            SqliteError::InvalidData(
                "writer connection busy (checkpoint skipped this tick)".to_string(),
            )
        })?;
        Ok(WriterGuard { guard })
    }

    /// Get the current number of available reader connections.
    pub fn available_readers(&self) -> usize {
        self.readers.len()
    }

    /// Get the total number of reader connections in the pool.
    pub fn max_readers(&self) -> usize {
        self.max_readers
    }

    /// Return the pool configuration.
    pub fn config(&self) -> &PoolConfig {
        &self.config
    }

    /// Compatibility method: returns the writer connection wrapped in `Arc<Mutex>`.
    ///
    /// WARNING: This exists only for backward compatibility with code that
    /// calls `store.conn()`. New code should use `reader()` and `writer()`.
    pub fn legacy_conn(&self) -> Arc<Mutex<Connection>> {
        Arc::clone(&self.writer)
    }

    fn open_reader_connection(&self) -> Result<Connection, SqliteError> {
        let path = self
            .config
            .path
            .as_ref()
            .expect("reader connections require a file-backed database");
        open_reader_connection(path, &self.config)
    }

    /// Open a standalone read-write connection to the same file-backed database.
    ///
    /// Stores whose trait methods take `Send + 'static` closures (executed via
    /// `spawn_blocking`) cannot hold the pooled `WriterGuard`'s `MutexGuard`
    /// across the call — it opens an independent connection instead. This
    /// must still honor `PoolConfig::read_only`: opening
    /// `SQLITE_OPEN_READ_WRITE` unconditionally here would let a read-only
    /// backend's graph/event/text stores bypass the flag that the pooled
    /// writer enforces via `query_only`.
    pub fn open_standalone_writer(&self) -> Result<Connection, SqliteError> {
        let path = self.config.path.as_ref().ok_or_else(|| {
            SqliteError::InvalidData(
                "in-memory databases do not support standalone connections".to_string(),
            )
        })?;

        if self.config.read_only {
            return Err(SqliteError::InvalidData(
                "database is read-only: standalone write connections are not permitted".to_string(),
            ));
        }

        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )?;
        conn.busy_timeout(self.config.busy_timeout)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(conn)
    }

    /// Open a standalone read-only connection to the same file-backed database.
    ///
    /// Companion to `open_standalone_writer` for stores that also need an
    /// independent reader connection outside the pooled reader queue.
    pub fn open_standalone_reader(&self) -> Result<Connection, SqliteError> {
        let path = self.config.path.as_ref().ok_or_else(|| {
            SqliteError::InvalidData(
                "in-memory databases do not support standalone connections".to_string(),
            )
        })?;

        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )?;
        conn.busy_timeout(self.config.busy_timeout)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Ok(conn)
    }

    fn return_reader(&self, conn: Connection) {
        if self.max_readers == 0 {
            return;
        }

        let conn = if reset_reader_connection(&conn) && reader_connection_is_healthy(&conn) {
            Some(conn)
        } else {
            close_connection_quietly(conn);
            self.open_reader_connection().ok()
        };

        if let Some(conn) = conn {
            if let Err(conn) = self.readers.push(conn) {
                eprintln!(
                    "[sqlite-pool] reader pool queue full, discarding replacement connection"
                );
                close_connection_quietly(conn);
            }
        }
    }
}

fn effective_reader_count(config: &PoolConfig, wal_enabled: bool) -> usize {
    if config.path.is_some() && config.wal_mode && wal_enabled {
        config.max_readers
    } else {
        0
    }
}

fn open_writer_connection(config: &PoolConfig) -> Result<Connection, SqliteError> {
    match config.path.as_ref() {
        Some(path) => {
            let flags = if config.read_only {
                writer_read_only_open_flags()
            } else {
                writer_open_flags()
            };
            Connection::open_with_flags(path, flags).map_err(Into::into)
        }
        None => Connection::open_in_memory().map_err(Into::into),
    }
}

fn open_reader_connection(path: &Path, config: &PoolConfig) -> Result<Connection, SqliteError> {
    let conn = Connection::open_with_flags(path, reader_open_flags())?;
    configure_reader_connection(&conn, config)?;
    Ok(conn)
}

fn writer_open_flags() -> OpenFlags {
    OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
}

/// Read-only writer-slot open flags: no `SQLITE_OPEN_CREATE`, so a missing
/// path is rejected rather than silently created.
fn writer_read_only_open_flags() -> OpenFlags {
    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX
}

fn reader_open_flags() -> OpenFlags {
    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX
}

fn configure_writer_connection(
    conn: &Connection,
    config: &PoolConfig,
) -> Result<bool, SqliteError> {
    if config.read_only {
        // Read-only writer slot: skip write-intent PRAGMAs (journal_mode,
        // wal_autocheckpoint, journal_size_limit all require write access to
        // change) and lock the connection down with query_only instead.
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(config.busy_timeout)?;
        conn.pragma_update(None, "cache_size", CACHE_SIZE_KIB)?;
        conn.pragma_update(None, "mmap_size", MMAP_SIZE_BYTES)?;
        conn.pragma_update(None, "temp_store", "MEMORY")?;
        conn.pragma_update(None, "query_only", "ON")?;

        let wal_enabled =
            config.wal_mode && current_journal_mode(conn)?.eq_ignore_ascii_case("wal");
        return Ok(wal_enabled);
    }

    let wants_wal = config.path.is_some() && config.wal_mode;

    if wants_wal {
        conn.pragma_update(None, "journal_mode", "WAL")?;
    }

    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(config.busy_timeout)?;
    conn.pragma_update(None, "cache_size", CACHE_SIZE_KIB)?;
    conn.pragma_update(None, "mmap_size", MMAP_SIZE_BYTES)?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;

    let wal_enabled = wants_wal && current_journal_mode(conn)?.eq_ignore_ascii_case("wal");

    if wal_enabled {
        conn.pragma_update(None, "wal_autocheckpoint", config.wal_autocheckpoint_pages)?;
        conn.pragma_update(None, "journal_size_limit", config.journal_size_limit_bytes)?;
    }

    Ok(wal_enabled)
}

fn configure_reader_connection(conn: &Connection, config: &PoolConfig) -> Result<(), SqliteError> {
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(config.busy_timeout)?;
    conn.pragma_update(None, "cache_size", CACHE_SIZE_KIB)?;
    conn.pragma_update(None, "mmap_size", MMAP_SIZE_BYTES)?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    Ok(())
}

fn current_journal_mode(conn: &Connection) -> Result<String, SqliteError> {
    conn.pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))
        .map(|mode| mode.to_ascii_lowercase())
        .map_err(Into::into)
}

fn reset_reader_connection(conn: &Connection) -> bool {
    if conn.is_autocommit() {
        return true;
    }

    match conn.execute_batch("ROLLBACK") {
        Ok(()) => conn.is_autocommit(),
        Err(rusqlite::Error::SqliteFailure(err, _)) => {
            if matches!(
                err.code,
                rusqlite::ErrorCode::CannotOpen
                    | rusqlite::ErrorCode::DatabaseCorrupt
                    | rusqlite::ErrorCode::NotADatabase
                    | rusqlite::ErrorCode::DiskFull
            ) {
                return false;
            }
            conn.is_autocommit()
        }
        Err(_) => false,
    }
}

fn reader_connection_is_healthy(conn: &Connection) -> bool {
    match conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0)) {
        Ok(_) => true,
        Err(rusqlite::Error::SqliteFailure(err, _)) => !matches!(
            err.code,
            rusqlite::ErrorCode::CannotOpen
                | rusqlite::ErrorCode::NotADatabase
                | rusqlite::ErrorCode::DatabaseCorrupt
                | rusqlite::ErrorCode::PermissionDenied
                | rusqlite::ErrorCode::SystemIoFailure
        ),
        Err(_) => true,
    }
}

fn close_connection_quietly(conn: Connection) {
    match conn.close() {
        Ok(()) => {}
        Err((conn, _)) => drop(conn),
    }
}

fn pool_exhausted_error(timeout: Duration, max_readers: usize) -> SqliteError {
    rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
        Some(format!(
            "Pool exhausted: no reader available after {timeout:?} (max_readers={max_readers})"
        )),
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn pool_config_default_values_match_constants() {
        // Ensure defaults are not accidentally changed.
        let cfg = PoolConfig::default();
        assert_eq!(
            cfg.wal_autocheckpoint_pages,
            DEFAULT_WAL_AUTOCHECKPOINT_PAGES
        );
        assert_eq!(
            cfg.journal_size_limit_bytes,
            DEFAULT_JOURNAL_SIZE_LIMIT_BYTES
        );
        assert_eq!(cfg.busy_timeout, Duration::from_secs(30));
        assert_eq!(cfg.checkout_timeout, Duration::from_secs(5));
    }

    #[test]
    #[serial]
    fn pool_config_env_override_wal_autocheckpoint() {
        std::env::set_var("KHIVE_WAL_AUTOCHECKPOINT_PAGES", "8000");
        let cfg = PoolConfig::default();
        std::env::remove_var("KHIVE_WAL_AUTOCHECKPOINT_PAGES");
        assert_eq!(cfg.wal_autocheckpoint_pages, 8000);
    }

    #[test]
    #[serial]
    fn pool_config_env_override_journal_size_limit() {
        std::env::set_var("KHIVE_JOURNAL_SIZE_LIMIT_BYTES", "134217728");
        let cfg = PoolConfig::default();
        std::env::remove_var("KHIVE_JOURNAL_SIZE_LIMIT_BYTES");
        assert_eq!(cfg.journal_size_limit_bytes, 134_217_728);
    }

    #[test]
    #[serial]
    fn pool_config_env_override_busy_timeout() {
        std::env::set_var("KHIVE_BUSY_TIMEOUT_SECS", "60");
        let cfg = PoolConfig::default();
        std::env::remove_var("KHIVE_BUSY_TIMEOUT_SECS");
        assert_eq!(cfg.busy_timeout, Duration::from_secs(60));
    }

    #[test]
    #[serial]
    fn pool_config_env_override_checkout_timeout() {
        std::env::set_var("KHIVE_CHECKOUT_TIMEOUT_SECS", "10");
        let cfg = PoolConfig::default();
        std::env::remove_var("KHIVE_CHECKOUT_TIMEOUT_SECS");
        assert_eq!(cfg.checkout_timeout, Duration::from_secs(10));
    }

    #[test]
    #[serial]
    fn pool_config_env_invalid_falls_back_to_default() {
        std::env::set_var("KHIVE_WAL_AUTOCHECKPOINT_PAGES", "not_a_number");
        std::env::set_var("KHIVE_JOURNAL_SIZE_LIMIT_BYTES", "");
        let cfg = PoolConfig::default();
        std::env::remove_var("KHIVE_WAL_AUTOCHECKPOINT_PAGES");
        std::env::remove_var("KHIVE_JOURNAL_SIZE_LIMIT_BYTES");
        assert_eq!(
            cfg.wal_autocheckpoint_pages,
            DEFAULT_WAL_AUTOCHECKPOINT_PAGES
        );
        assert_eq!(
            cfg.journal_size_limit_bytes,
            DEFAULT_JOURNAL_SIZE_LIMIT_BYTES
        );
    }

    #[test]
    fn file_backed_pool_opens_successfully() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test_pool.db");
        let cfg = PoolConfig {
            path: Some(path.clone()),
            ..PoolConfig::default()
        };
        let pool = ConnectionPool::new(cfg).expect("file-backed pool should open");
        assert!(path.exists());
        assert!(pool.max_readers() > 0);
    }

    #[test]
    fn in_memory_pool_degrades_to_single_connection() {
        let cfg = PoolConfig {
            path: None,
            ..PoolConfig::default()
        };
        let pool = ConnectionPool::new(cfg).expect("in-memory pool should open");
        assert_eq!(pool.max_readers(), 0);
    }

    #[test]
    fn writer_checkout_and_release_works() {
        let cfg = PoolConfig {
            path: None,
            ..PoolConfig::default()
        };
        let pool = ConnectionPool::new(cfg).unwrap();
        {
            let _writer = pool.writer().expect("writer checkout should succeed");
        }
        // After drop, writer should be re-acquirable.
        let _writer2 = pool
            .writer()
            .expect("second writer checkout should succeed");
    }

    /// ADR-091 Plank 0: `WriterGuard::transaction` registers an entry with the
    /// shared open-transaction registry for the duration of the closure, and
    /// deregisters it once the closure (and its commit/rollback) completes.
    ///
    /// `#[serial(tx_registry)]`: the open-transaction registry is a
    /// process-wide singleton (`khive_storage::tx_registry`) shared across
    /// every test in this binary. This test filters by its own unique label
    /// so it is not vulnerable to another test's entry being reported as
    /// "oldest", but it still shares the same `tx_registry` serial group as
    /// `checkpoint.rs`'s and `sql_bridge.rs`'s registry tests for
    /// defense-in-depth against cross-test interference.
    #[test]
    #[serial(tx_registry)]
    fn writer_guard_transaction_registers_during_closure_only() {
        let cfg = PoolConfig {
            path: None,
            ..PoolConfig::default()
        };
        let pool = ConnectionPool::new(cfg).unwrap();
        let guard = pool.writer().unwrap();

        let mut seen_during_closure = false;
        let result: Result<(), SqliteError> = guard.transaction(|_conn| {
            seen_during_closure = khive_storage::tx_registry::snapshot()
                .iter()
                .any(|(_, label)| label.as_deref() == Some("writer_guard_tx"));
            Ok(())
        });
        result.expect("transaction should commit");

        assert!(
            seen_during_closure,
            "expected a writer_guard_tx entry visible inside the closure"
        );
        assert!(
            !khive_storage::tx_registry::snapshot()
                .iter()
                .any(|(_, label)| label.as_deref() == Some("writer_guard_tx")),
            "expected the entry to be gone after the transaction completes"
        );
    }
}
