use std::sync::Arc;

use super::{checkpoint_once_core, CheckpointConfig, CheckpointCoreOutcome, TruncateState};
use crate::pool::ConnectionPool;

/// Run one synchronous checkpoint cycle off this task's Tokio worker thread.
///
/// The cycle issues the PASSIVE observation and, when armed, the TRUNCATE
/// escalation. TRUNCATE lowers the connection's busy timeout and then waits on
/// SQLite for up to `config.truncate_busy_timeout` while a reader pins the
/// WAL, which would hold a worker thread for that whole wait if it ran inline.
/// `conn` and `truncate_state` are moved into the blocking closure and handed
/// back to the caller whenever the cycle returns, so the checkpoint task can
/// restore its dedicated connection and escalation state on every
/// non-panicking path. A panic inside the cycle surfaces as the `JoinError`:
/// the connection and escalation state moved into the task are gone with it,
/// and the caller reopens the connection and starts a fresh state instead of
/// taking the checkpoint task down.
pub(super) async fn run_checkpoint_core_off_worker(
    pool: Arc<ConnectionPool>,
    conn: rusqlite::Connection,
    config: CheckpointConfig,
    mut truncate_state: TruncateState,
) -> Result<
    (
        rusqlite::Connection,
        TruncateState,
        Result<CheckpointCoreOutcome, rusqlite::Error>,
    ),
    tokio::task::JoinError,
> {
    tokio::task::spawn_blocking(move || {
        let outcome = checkpoint_once_core(&pool, &conn, &config, &mut truncate_state);
        (conn, truncate_state, outcome)
    })
    .await
}
