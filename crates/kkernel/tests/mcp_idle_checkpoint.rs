//! A completed real stdio read must not retain a snapshot while its bridge is idle.
//! The same PASSIVE instrument is first calibrated against an actual held reader.

#![cfg(unix)]

use std::fs::{self, File};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, ensure, Context, Result};
use khive_db::pool::{CheckpointResult, StandaloneCheckpoint};
use khive_db::stores::entity::SqlEntityStore;
use khive_db::{ConnectionPool, PoolConfig};
use khive_storage::entity::Entity;
use khive_storage::EntityStore;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::ServiceExt;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use tempfile::TempDir;
use tokio::process::{Child, Command};
use tokio::time::{sleep, timeout, Instant};
use uuid::Uuid;

const STARTUP_LIMIT: Duration = Duration::from_secs(60);
const REQUEST_LIMIT: Duration = Duration::from_secs(60);
const CHECKPOINT_LIMIT: Duration = Duration::from_secs(10);
const CLEANUP_LIMIT: Duration = Duration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const LOG_LIMIT: u64 = 1024 * 1024;
const TERMINAL_EVENT: &str = "admitted daemon forward reached a terminal outcome";

#[derive(Clone, Copy)]
enum Route {
    Local,
    Forwarded,
}

struct Case {
    directory: TempDir,
    db: PathBuf,
    socket: PathBuf,
    bridge_log: PathBuf,
    daemon_log: PathBuf,
    bridge: Option<Child>,
    daemon: Option<Child>,
}

impl Case {
    fn new() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
        Ok(Self {
            db: directory.path().join("main.db"),
            socket: directory.path().join("khived.sock"),
            bridge_log: directory.path().join("bridge.stderr"),
            daemon_log: directory.path().join("daemon.stderr"),
            directory,
            bridge: None,
            daemon: None,
        })
    }

    fn command(&self, daemon: bool, route: Route) -> Result<Command> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kkernel"));
        // Keep loader/PATH settings supplied by Cargo, but no operator khive config.
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("KHIVE_") {
                command.env_remove(key);
            }
        }
        command
            .args(["--log", "debug", "mcp", "--db"])
            .arg(&self.db)
            .args(["--no-embed", "--pack", "kg", "--actor", "local"])
            .current_dir(self.directory.path())
            .env("HOME", self.directory.path())
            .env(
                "KHIVE_VOLUME_LOCK_DIR",
                self.directory.path().join("volume-locks"),
            )
            .env("KHIVE_SOCKET", &self.socket)
            .env("KHIVE_PID", self.directory.path().join("khived.pid"))
            .env(
                "KHIVE_LOCK",
                self.directory.path().join("khived.recovery.lock"),
            )
            .env("KHIVE_EVENTS_SPLIT", "0")
            .env("KHIVE_WRITE_QUEUE", "0")
            .env("KHIVE_WAL_TRUNCATE_HIGH_WATER_PAGES", u64::MAX.to_string())
            .kill_on_drop(true);
        if daemon {
            command
                .args(["--daemon", "--lifetime", "persistent"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::from(File::create(&self.daemon_log)?));
        } else {
            if matches!(route, Route::Local) {
                command.env("KHIVE_NO_DAEMON", "1");
            }
            command
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::from(File::create(&self.bridge_log)?));
        }
        Ok(command)
    }

    fn ensure_alive(&mut self) -> Result<()> {
        ensure!(
            self.bridge
                .as_mut()
                .context("bridge was not spawned")?
                .try_wait()?
                .is_none(),
            "bridge exited during the checkpoint witness"
        );
        if let Some(daemon) = &mut self.daemon {
            ensure!(
                daemon.try_wait()?.is_none(),
                "owned daemon exited during the witness"
            );
        }
        Ok(())
    }

    async fn cleanup(&mut self) -> Result<()> {
        let mut failures = Vec::new();
        if let Some(bridge) = &mut self.bridge {
            if let Err(error) = reap(bridge, false).await {
                failures.push(format!("bridge cleanup: {error:#}"));
            }
        }
        if let Some(daemon) = &mut self.daemon {
            if let Err(error) = reap(daemon, true).await {
                failures.push(format!("daemon cleanup: {error:#}"));
            }
        }
        ensure!(failures.is_empty(), "{}", failures.join("; "));
        Ok(())
    }
}

async fn reap(child: &mut Child, terminate: bool) -> Result<()> {
    let mut failures = Vec::new();
    match child.try_wait() {
        Ok(Some(status)) => {
            ensure!(
                status.success(),
                "owned child already exited unsuccessfully: {status}"
            );
            return Ok(());
        }
        Ok(None) => {}
        Err(error) => failures.push(format!("initial child status: {error}")),
    }
    if terminate {
        if let Some(pid) = child.id() {
            let pid = libc::pid_t::try_from(pid).context("daemon PID does not fit pid_t")?;
            // SAFETY: this is the still-owned, not-yet-reaped child PID.
            let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
            if rc != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    failures.push(format!("SIGTERM owned daemon: {error}"));
                }
            }
        }
    }
    match timeout(CLEANUP_LIMIT, child.wait()).await {
        Ok(Ok(status)) => {
            if !status.success() {
                failures.push(format!("owned child exited unsuccessfully: {status}"));
            }
        }
        first => {
            failures.push(format!("graceful child wait: {first:?}"));
            if let Err(error) = child.start_kill() {
                failures.push(format!("kill after bounded wait: {error}"));
            }
            match timeout(CLEANUP_LIMIT, child.wait()).await {
                Ok(Ok(_)) => {}
                final_wait => failures.push(format!("child was not reaped: {final_wait:?}")),
            }
        }
    }
    ensure!(failures.is_empty(), "{}", failures.join("; "));
    Ok(())
}

fn read_log(path: &Path) -> Result<String> {
    if !path.exists() {
        return Ok(String::new());
    }
    ensure!(
        fs::metadata(path)?.len() <= LOG_LIMIT,
        "child log exceeded bounded capture: {}",
        path.display()
    );
    fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
}

fn integer_field(line: &str, name: &str) -> Option<u64> {
    let prefix = format!("{name}=");
    line.split_whitespace()
        .find_map(|word| word.strip_prefix(&prefix))?
        .parse()
        .ok()
}

async fn wait_for_daemon(case: &mut Case) -> Result<()> {
    let deadline = Instant::now() + STARTUP_LIMIT;
    let child = case.daemon.as_mut().context("daemon was not spawned")?;
    let pid = u64::from(child.id().context("daemon has no PID")?);
    loop {
        ensure!(
            child.try_wait()?.is_none(),
            "owned daemon exited before readiness"
        );
        let log = read_log(&case.daemon_log)?;
        if case.socket.exists()
            && log.lines().any(|line| {
                line.contains("khived listening") && integer_field(line, "pid") == Some(pid)
            })
        {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "owned daemon did not announce its listening PID/socket: {log}"
        );
        sleep(POLL_INTERVAL).await;
    }
}

fn request(ops: &str) -> CallToolRequestParams {
    let mut arguments = serde_json::Map::new();
    arguments.insert("ops".into(), Value::String(ops.into()));
    arguments.insert("format".into(), Value::String("json".into()));
    arguments.insert("presentation".into(), Value::String("verbose".into()));
    CallToolRequestParams::new("request").with_arguments(arguments)
}

fn result_value(response: CallToolResult) -> Result<Value> {
    ensure!(
        response.is_error != Some(true),
        "MCP call returned an error: {response:?}"
    );
    let text = response
        .content
        .first()
        .and_then(|content| content.as_text())
        .context("MCP response has no text content")?;
    let envelope: Value = serde_json::from_str(&text.text).context("MCP response is not JSON")?;
    ensure!(
        envelope["status"] == "success",
        "request failed: {envelope}"
    );
    let results = envelope["results"]
        .as_array()
        .context("request has no results array")?;
    ensure!(
        results.len() == 1 && results[0]["ok"] == true,
        "expected one successful result: {envelope}"
    );
    ensure!(
        results[0]["result"].is_object(),
        "result is not an object: {envelope}"
    );
    Ok(results[0]["result"].clone())
}

async fn committed_entity(store: &SqlEntityStore, label: &str) -> Result<Entity> {
    let entity = Entity::new(
        "local",
        "concept",
        format!("idle-checkpoint-{label}-{}", Uuid::new_v4()),
    )
    .with_description(format!("committed marker {label}"));
    store.upsert_entity(entity.clone()).await?;
    Ok(entity)
}

fn full_nonzero(row: CheckpointResult) -> bool {
    row.busy == 0 && row.log_frames > 0 && row.checkpointed_frames == row.log_frames
}

async fn require_full(
    checkpoint: &StandaloneCheckpoint<'_>,
    phase: &str,
) -> Result<CheckpointResult> {
    let deadline = Instant::now() + CHECKPOINT_LIMIT;
    loop {
        let row = checkpoint
            .passive()
            .with_context(|| format!("{phase}: PASSIVE failed"))?;
        if full_nonzero(row) {
            eprintln!("{phase}: {row:?}");
            return Ok(row);
        }
        ensure!(
            Instant::now() < deadline,
            "{phase}: no valid nonzero full backfill; last={row:?}"
        );
        sleep(POLL_INTERVAL).await;
    }
}

async fn calibrate_pin(
    checkpoint: &StandaloneCheckpoint<'_>,
    store: &SqlEntityStore,
    db: &Path,
) -> Result<()> {
    committed_entity(store, "healthy-baseline").await?;
    require_full(checkpoint, "healthy baseline").await?;
    let mut observations = Vec::new();
    for attempt in 0..3 {
        // A fresh committed row must actually be read before later commits.
        let marker = committed_entity(store, &format!("snapshot-{attempt}")).await?;
        let snapshot = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        snapshot.execute_batch("BEGIN DEFERRED")?;
        let stored: String = snapshot.query_row(
            "SELECT description FROM entities WHERE id = ?1",
            [marker.id.to_string()],
            |row| row.get(0),
        )?;
        ensure!(
            Some(stored.as_str()) == marker.description.as_deref(),
            "snapshot did not read the committed marker"
        );
        committed_entity(store, &format!("held-a-{attempt}")).await?;
        committed_entity(store, &format!("held-b-{attempt}")).await?;
        let row = checkpoint.passive()?;
        observations.push(row);
        if row.busy == 0
            && row.log_frames > 0
            && row.checkpointed_frames >= 0
            && row.checkpointed_frames < row.log_frames
        {
            eprintln!("held snapshot positive attempt={attempt}: {row:?}");
            // The positive reader must be explicitly released before catch-up.
            snapshot.execute_batch("ROLLBACK")?;
            drop(snapshot);
            require_full(checkpoint, "calibration release catch-up").await?;
            return Ok(());
        }
        snapshot.execute_batch("ROLLBACK")?;
        drop(snapshot);
    }
    bail!("same PASSIVE instrument did not observe a held-snapshot gap: {observations:?}")
}

async fn require_route(case: &Case, route: Route) -> Result<()> {
    let deadline = Instant::now() + REQUEST_LIMIT;
    let expected = match route {
        Route::Local => "daemon_outcome_present=false",
        Route::Forwarded => "daemon_outcome_present=true",
    };
    loop {
        let log = read_log(&case.bridge_log)?;
        let terminal: Vec<_> = log
            .lines()
            .filter(|line| line.contains(TERMINAL_EVENT))
            .collect();
        if !terminal.is_empty() {
            ensure!(
                terminal.len() == 1,
                "one store request produced {} terminal route events: {log}",
                terminal.len()
            );
            ensure!(
                terminal[0].contains(expected),
                "unexpected actual route: {}",
                terminal[0]
            );
            ensure!(
                integer_field(terminal[0], "request_id").is_some_and(|id| id > 0),
                "route event has no nonzero request ID: {}",
                terminal[0]
            );
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "completed get has no terminal route evidence: {log}"
        );
        sleep(POLL_INTERVAL).await;
    }
}

async fn exercise(case: &mut Case, route: Route) -> Result<()> {
    if matches!(route, Route::Forwarded) {
        case.daemon = Some(
            case.command(true, route)?
                .spawn()
                .context("spawn owned real daemon")?,
        );
        wait_for_daemon(case).await?;
    }
    case.bridge = Some(
        case.command(false, route)?
            .spawn()
            .context("spawn stdio bridge")?,
    );
    let bridge = case.bridge.as_mut().context("bridge was not stored")?;
    let bridge_pid = u64::from(bridge.id().context("bridge has no PID")?);
    let stdout = bridge
        .stdout
        .take()
        .context("bridge stdout was not piped")?;
    let stdin = bridge.stdin.take().context("bridge stdin was not piped")?;
    let client = timeout(STARTUP_LIMIT, ().serve((stdout, stdin)))
        .await
        .context("stdio handshake timed out")?
        .context("stdio handshake failed")?;

    let body: Result<()> = async {
        let pool = Arc::new(ConnectionPool::new(PoolConfig {
            path: Some(case.db.clone()),
            write_queue_enabled: Some(false),
            write_routing_strict: false,
            volume_lock_dir: Some(case.directory.path().join("volume-locks")),
            ..PoolConfig::for_test()
        })?);
        pool.claim_checkpoint_ownership()?;
        let checkpoint = pool.open_standalone_checkpoint()?;
        let store = SqlEntityStore::new(pool.clone(), true);
        let seed = committed_entity(&store, "public-get").await?;
        calibrate_pin(&checkpoint, &store, &case.db).await?;
        case.ensure_alive()?;
        let before = result_value(
            timeout(
                REQUEST_LIMIT,
                client.call_tool(request("bridge.diagnostics()")),
            )
            .await
            .context("initial diagnostics timed out")??,
        )?;
        ensure!(
            before["pid"].as_u64() == Some(bridge_pid),
            "diagnostics PID is not the owned bridge: {before}"
        );
        let generation = before["bridge_instance_id"]
            .as_str()
            .context("diagnostics has no generation UUID")?;
        Uuid::parse_str(generation).context("diagnostics generation is not a UUID")?;

        // This is the only store-bearing request sent through the bridge.
        let get = result_value(
            timeout(
                REQUEST_LIMIT,
                client.call_tool(request(&format!("get(id=\"{}\")", seed.id))),
            )
            .await
            .context("public get timed out")??,
        )?;
        ensure!(
            get["id"].as_str() == Some(seed.id.to_string().as_str()),
            "get did not return the full seeded UUID: {get}"
        );
        ensure!(
            get["name"].as_str() == Some(seed.name.as_str()),
            "get returned the wrong name: {get}"
        );
        ensure!(
            get["description"].as_str() == seed.description.as_deref(),
            "get returned the wrong committed description: {get}"
        );
        require_route(case, route).await?;
        case.ensure_alive()?;

        // No more bridge store requests: only independent, committed parent writes.
        for index in 0..3 {
            committed_entity(&store, &format!("idle-{index}")).await?;
            require_full(&checkpoint, "idle bridge fresh-commit catch-up").await?;
            case.ensure_alive()?;
        }
        let after = result_value(
            timeout(
                REQUEST_LIMIT,
                client.call_tool(request("bridge.diagnostics()")),
            )
            .await
            .context("final diagnostics timed out")??,
        )?;
        ensure!(
            after["pid"] == before["pid"]
                && after["bridge_instance_id"] == before["bridge_instance_id"],
            "bridge changed process image: before={before}, after={after}"
        );
        case.ensure_alive()?;
        Ok(())
    }
    .await;

    // Closing the tuple transport is separate from reaping our owned process.
    let close = timeout(CLEANUP_LIMIT, client.cancel())
        .await
        .context("rmcp cancellation timed out")
        .and_then(|result| result.context("rmcp cancellation task failed"));
    match (body, close) {
        (Ok(()), Ok(_)) => Ok(()),
        (Err(error), Ok(_)) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(body), Err(close)) => Err(anyhow!(
            "case failed: {body:#}; rmcp cleanup failed: {close:#}"
        )),
    }
}

async fn run_case(route: Route) -> Result<()> {
    let mut case = Case::new()?;
    let body = exercise(&mut case, route).await;
    let cleanup = case.cleanup().await;
    if body.is_err() || cleanup.is_err() {
        let logs = format!(
            "bridge stderr: {}\ndaemon stderr: {}",
            read_log(&case.bridge_log).unwrap_or_else(|error| format!("{error:#}")),
            read_log(&case.daemon_log).unwrap_or_else(|error| format!("{error:#}"))
        );
        let retained_path = case.directory.path().to_path_buf();
        // Preserve failed-case files, especially if a child could not be reaped.
        // Remaining Child fields still drop with kill_on_drop as a last resort.
        std::mem::forget(case.directory);
        bail!(
            "case={body:?}; cleanup={cleanup:?}; retained={}\n{logs}",
            retained_path.display()
        );
    }
    Ok(())
}

#[tokio::test]
async fn completed_local_stdio_read_does_not_pin_wal_while_bridge_is_idle() {
    if let Err(error) = run_case(Route::Local).await {
        panic!("{error:#}");
    }
}

#[tokio::test]
async fn completed_forwarded_stdio_read_does_not_pin_wal_while_bridge_is_idle() {
    if let Err(error) = run_case(Route::Forwarded).await {
        panic!("{error:#}");
    }
}
