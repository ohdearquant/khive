//! The stock MCP daemon checks forced indexes before publishing its rendezvous.

#![cfg(unix)]

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Result};
use khive_db::StorageBackend;
use rusqlite::{Connection, OpenFlags};
use tempfile::TempDir;

const FORCED_INDEX: &str = "idx_graph_edges_ns_src_rel";
const LISTENING: &str = "khived listening";
const STARTUP_LIMIT: Duration = Duration::from_secs(60);

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[derive(Debug)]
enum Startup {
    Listening,
    Exited(ExitStatus),
}

fn fixture() -> Result<TempDir> {
    let scratch = Path::new("/tmp").canonicalize()?;
    let root = tempfile::Builder::new()
        .prefix("sir-")
        .tempdir_in(scratch)?;
    fs::write(root.path().join("khive.toml"), "engines = []\n")?;
    Ok(root)
}

fn seed_database(root: &Path, db: &Path) -> Result<()> {
    let backend = StorageBackend::sqlite_for_test_with_journal_mode_in(
        db,
        false,
        Duration::from_secs(2),
        root.join("volume-locks"),
    )?;
    backend.prepare_core_schema()?;
    backend.prepare_serving_store_schemas()?;
    Ok(())
}

fn has_forced_index(db: &Path) -> Result<bool> {
    let reader = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(reader.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'index' AND name = ?1)",
        [FORCED_INDEX],
        |row| row.get(0),
    )?)
}

fn drop_forced_index(db: &Path) -> Result<()> {
    Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_WRITE)?
        .execute_batch("DROP INDEX idx_graph_edges_ns_src_rel")?;
    ensure!(!has_forced_index(db)?, "failed to damage the test fixture");
    Ok(())
}

fn named_backends(root: &Path, secondary_read_only: bool) -> Result<[PathBuf; 2]> {
    let main = root.join("main.db");
    let secondary = root.join("secondary.db");
    seed_database(root, &main)?;
    seed_database(root, &secondary)?;
    fs::write(
        root.join("khive.toml"),
        format!(
            "engines = []\n\n\
             [[backends]]\nname = \"main\"\nkind = \"sqlite\"\npath = {main:?}\n\n\
             [[backends]]\nname = \"archive\"\nkind = \"sqlite\"\npath = {secondary:?}\n\
             read_only = {secondary_read_only}\n"
        ),
    )?;
    Ok([main, secondary])
}

fn stderr(root: &Path) -> Result<String> {
    Ok(fs::read_to_string(root.join("daemon.stderr"))?)
}

fn spawn_daemon(root: &Path, db: Option<&Path>) -> Result<Daemon> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_kkernel"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("KHIVE_") {
            command.env_remove(key);
        }
    }
    command
        .args([
            "mcp",
            "--daemon",
            "--lifetime",
            "persistent",
            "--no-embed",
            "--pack",
            "kg",
            "--config",
        ])
        .arg(root.join("khive.toml"))
        .current_dir(root)
        .env("HOME", root.join("home"))
        .env_remove("LATTICE_MODEL_CACHE")
        .env("KHIVE_TEST_HARNESS", "1")
        .env("KHIVE_LOG", "info")
        .env("KHIVE_VOLUME_LOCK_DIR", root.join("volume-locks"))
        .env("KHIVE_WRITER_TIMEOUT_SINK_DIR", root.join("writer-logs"))
        .env("KHIVE_SOCKET", root.join("host.sock"))
        .env("KHIVE_PID", root.join("host.pid"))
        .env("KHIVE_LOCK", root.join("boot.lock"))
        .env("KHIVE_EVENTS_SPLIT", "0")
        .env("KHIVE_SQLITE_WAL_CEILING_BYTES", "0")
        .env("KHIVE_SQLITE_DISK_RESERVE_BYTES", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(File::create(root.join("daemon.stderr"))?));
    if let Some(db) = db {
        command.arg("--db").arg(db);
    }
    Ok(Daemon(command.spawn()?))
}

fn wait_for_startup(root: &Path, daemon: &mut Daemon) -> Result<Startup> {
    let deadline = Instant::now() + STARTUP_LIMIT;
    loop {
        if let Some(status) = daemon.0.try_wait()? {
            return Ok(Startup::Exited(status));
        }
        if root.join("host.sock").exists()
            && root.join("host.pid").exists()
            && stderr(root)?.contains(LISTENING)
        {
            return Ok(Startup::Listening);
        }
        ensure!(
            Instant::now() < deadline,
            "daemon startup timed out:\n{}",
            stderr(root)?
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn expect_listening(root: &Path, daemon: &mut Daemon) -> Result<()> {
    match wait_for_startup(root, daemon)? {
        Startup::Listening => Ok(()),
        Startup::Exited(status) => bail!(
            "daemon exited before listening ({status}):\n{}",
            stderr(root)?
        ),
    }
}

#[test]
fn single_writable_store_repairs_a_forced_index_before_daemon_readiness() -> Result<()> {
    let fixture = fixture()?;
    let root = fixture.path();
    let db = root.join("main.db");
    seed_database(root, &db)?;
    drop_forced_index(&db)?;

    let mut daemon = spawn_daemon(root, Some(&db))?;
    expect_listening(root, &mut daemon)?;
    ensure!(
        has_forced_index(&db)?,
        "missing {FORCED_INDEX} at readiness on main"
    );
    Ok(())
}

#[test]
fn unassigned_writable_store_repairs_a_forced_index_before_daemon_readiness() -> Result<()> {
    let fixture = fixture()?;
    let root = fixture.path();
    let [main, secondary] = named_backends(root, false)?;
    drop_forced_index(&main)?;
    drop_forced_index(&secondary)?;

    let mut daemon = spawn_daemon(root, None)?;
    expect_listening(root, &mut daemon)?;
    for db in [&main, &secondary] {
        ensure!(
            has_forced_index(db)?,
            "missing {FORCED_INDEX} at readiness on {}",
            db.display()
        );
    }
    Ok(())
}

#[test]
fn unassigned_read_only_store_with_a_missing_forced_index_refuses_daemon_boot() -> Result<()> {
    let fixture = fixture()?;
    let root = fixture.path();
    let [_main, secondary] = named_backends(root, true)?;
    drop_forced_index(&secondary)?;
    let snapshot = fs::read(&secondary)?;

    let mut daemon = spawn_daemon(root, None)?;
    let startup = wait_for_startup(root, &mut daemon)?;
    let log = stderr(root)?;
    ensure!(
        matches!(startup, Startup::Exited(status) if !status.success()),
        "daemon did not refuse the damaged snapshot ({startup:?}):\n{log}"
    );
    ensure!(
        !root.join("host.pid").exists()
            && !root.join("host.sock").exists()
            && !log.contains(LISTENING),
        "daemon published readiness for the damaged snapshot:\n{log}"
    );
    let version = khive_db::migrations::latest_schema_version();
    ensure!(
        log.contains(&format!("backend archive schema version {version}"))
            && log.contains("read-only database is missing forced index")
            && log.contains(FORCED_INDEX),
        "refusal did not name the affected backend and index:\n{log}"
    );
    ensure!(
        !has_forced_index(&secondary)?,
        "read-only index was rebuilt"
    );
    ensure!(
        fs::read(&secondary)? == snapshot,
        "read-only snapshot changed on disk"
    );
    Ok(())
}
