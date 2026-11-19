//! Write latency under the volume lease: two processes, two databases, one volume.
//!
//! A manual measurement, not a gate. Run it on the volume under test:
//! `KHIVE_LEASE_LATENCY_DIR=<absolute dir> cargo test -p khive-db --lib lease_latency
//! -- --ignored --nocapture --test-threads=1`.
//!
//! Each round runs three arms back to back, in an order rotated every round so
//! drift cannot settle on one arm:
//! - `shared`: both writers lock one directory, as production does;
//! - `private`: each writer locks its own directory, so the lease is taken but
//!   never contended between the two processes;
//! - `none`: each writer takes an unheld lease, so no lock call is made.
//!
//! `shared` minus `none` is the lease's cost. `private` splits that cost into
//! the lock calls themselves and cross-process serialization.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::disk_guard::SKIP_VOLUME_LEASE_FOR_MEASUREMENT;
use crate::pool::{ConnectionPool, PoolConfig};

const DIR_ENV: &str = "KHIVE_LEASE_LATENCY_DIR";
const ROLE_ENV: &str = "KHIVE_LEASE_LATENCY_ROLE";
const ARM_ENV: &str = "KHIVE_LEASE_LATENCY_ARM";
const RUN_ENV: &str = "KHIVE_LEASE_LATENCY_RUN";
const ROUNDS: usize = 6;
const WRITES: usize = 2000;
const GAP: Duration = Duration::from_micros(500);
const ARMS: [&str; 3] = ["shared", "private", "none"];
/// Row payload per writer: a main-store note and an event row.
const ROLES: [(&str, usize); 2] = [("main", 1024), ("events", 192)];
const LOCK_PREFIX: &str = "sqlite-volume-v1-";

#[test]
#[ignore = "manual measurement: set KHIVE_LEASE_LATENCY_DIR to a dir on the volume under test"]
fn two_process_write_latency_with_and_without_the_lease() {
    if let Ok(role) = std::env::var(ROLE_ENV) {
        write_as_child(&role);
        return;
    }
    let Some(root) = std::env::var_os(DIR_ENV).map(PathBuf::from) else {
        eprintln!(
            "SKIP LEASE_LATENCY: set {DIR_ENV} to an absolute directory \
             on the volume under test"
        );
        return;
    };
    assert!(root.is_absolute(), "{DIR_ENV} must be absolute");
    let name = std::thread::current()
        .name()
        .expect("libtest names its test threads")
        .to_string();
    println!(
        "LEASE_LATENCY dir={} rounds={ROUNDS} writes={WRITES} gap_us={}",
        root.display(),
        GAP.as_micros()
    );
    println!("LEASE_LATENCY load_before={}", load_average());
    let mut samples: BTreeMap<(&str, &str), Vec<u128>> = BTreeMap::new();
    for round in 0..ROUNDS {
        let mut order = Vec::new();
        for offset in 0..ARMS.len() {
            let arm = ARMS[(round + offset) % ARMS.len()];
            order.push(arm);
            let run = root.join(format!("round{round}-{arm}"));
            std::fs::create_dir_all(&run).expect("run directory");
            let children: Vec<(&str, Child)> = ROLES
                .iter()
                .map(|(role, _)| (*role, spawn_writer(&name, &run, role, arm)))
                .collect();
            wait_for_ready(&run);
            std::fs::write(run.join("go"), b"").expect("start signal");
            for (role, mut child) in children {
                let status = child.wait().expect("writer child exit");
                assert!(
                    status.success(),
                    "{role} writer failed in round {round}, arm {arm}"
                );
                samples
                    .entry((arm, role))
                    .or_default()
                    .extend(read_latencies(&run.join(format!("{role}.lat"))));
            }
            assert_lock_population(&run, arm);
            std::fs::remove_dir_all(&run).expect("remove run directory");
        }
        println!("LEASE_LATENCY round={round} order={}", order.join(","));
    }
    println!("LEASE_LATENCY load_after={}", load_average());
    for ((arm, role), mut values) in samples {
        values.sort_unstable();
        println!(
            "LEASE_LATENCY arm={arm} role={role} n={} p50_us={} p99_us={} max_us={}",
            values.len(),
            percentile(&values, 0.50),
            percentile(&values, 0.99),
            values[values.len() - 1],
        );
    }
}

fn write_as_child(role: &str) {
    let run = PathBuf::from(std::env::var_os(RUN_ENV).expect("run directory"));
    let arm = std::env::var(ARM_ENV).expect("arm");
    let payload = ROLES
        .iter()
        .find(|(name, _)| *name == role)
        .expect("known role")
        .1;
    let locks = if arm == "private" {
        run.join(format!("locks-{role}"))
    } else {
        run.join("locks")
    };
    if arm == "none" {
        SKIP_VOLUME_LEASE_FOR_MEASUREMENT.store(true, Ordering::Relaxed);
    }
    let pool = ConnectionPool::new(PoolConfig {
        path: Some(run.join(format!("{role}.db"))),
        volume_lock_dir: Some(locks),
        disk_guard_config: Some(
            crate::DiskGuardEnvironment::default()
                .resolve(Some(0), Some(10_000))
                .expect("disk guard policy"),
        ),
        write_queue_enabled: Some(false),
        ..PoolConfig::for_test()
    })
    .expect("open writer pool");
    pool.writer()
        .and_then(|writer| {
            writer.transaction(|conn| {
                conn.execute_batch("CREATE TABLE w (id INTEGER PRIMARY KEY, body BLOB NOT NULL)")?;
                Ok(())
            })
        })
        .expect("create table");
    std::fs::write(run.join(format!("{role}.ready")), b"").expect("ready signal");
    while !run.join("go").exists() {
        std::thread::sleep(Duration::from_millis(1));
    }
    let body = vec![0x5a_u8; payload];
    let mut latencies = Vec::with_capacity(WRITES);
    for _ in 0..WRITES {
        let started = Instant::now();
        pool.writer()
            .and_then(|writer| {
                writer.transaction(|conn| {
                    conn.execute("INSERT INTO w (body) VALUES (?1)", [&body])?;
                    Ok(())
                })
            })
            .expect("measured write");
        latencies.push(started.elapsed().as_micros().to_string());
        std::thread::sleep(GAP);
    }
    latencies.push(String::new());
    std::fs::write(run.join(format!("{role}.lat")), latencies.join("\n")).expect("latency file");
}

fn spawn_writer(name: &str, run: &Path, role: &str, arm: &str) -> Child {
    Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", name, "--include-ignored", "--test-threads=1"])
        .env(ROLE_ENV, role)
        .env(ARM_ENV, arm)
        .env(RUN_ENV, run)
        .stdout(Stdio::null())
        .spawn()
        .expect("spawn writer child")
}

fn wait_for_ready(run: &Path) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while ROLES
        .iter()
        .any(|(role, _)| !run.join(format!("{role}.ready")).exists())
    {
        assert!(
            Instant::now() < deadline,
            "writers did not open their pools within 60 s"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn read_latencies(path: &Path) -> Vec<u128> {
    let text = std::fs::read_to_string(path).expect("latency file");
    let values: Vec<u128> = text
        .lines()
        .map(|line| line.parse().expect("latency value"))
        .collect();
    assert_eq!(
        values.len(),
        WRITES,
        "{} must hold one latency per write",
        path.display()
    );
    values
}

fn lock_files(dir: &Path) -> usize {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .map(|entry| entry.expect("lock directory entry"))
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(LOCK_PREFIX))
            .count(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => panic!("read {}: {error}", dir.display()),
    }
}

/// Each arm must have taken exactly the locks it names; a switch that did
/// nothing would otherwise measure the shared arm three times.
fn assert_lock_population(run: &Path, arm: &str) {
    match arm {
        "shared" => assert_eq!(
            lock_files(&run.join("locks")),
            1,
            "both writers lock one volume file"
        ),
        "private" => {
            for (role, _) in ROLES {
                assert_eq!(
                    lock_files(&run.join(format!("locks-{role}"))),
                    1,
                    "{role} locks its own directory"
                );
            }
            assert_eq!(lock_files(&run.join("locks")), 0, "no shared lock file");
        }
        "none" => assert_eq!(
            lock_files(&run.join("locks")),
            0,
            "the no-lease arm takes no file lock"
        ),
        other => panic!("unknown arm {other}"),
    }
}

fn percentile(sorted: &[u128], fraction: f64) -> u128 {
    let index = ((sorted.len() - 1) as f64 * fraction).round() as usize;
    sorted[index]
}

fn load_average() -> String {
    if let Ok(text) = std::fs::read_to_string("/proc/loadavg") {
        return text
            .split_whitespace()
            .take(3)
            .collect::<Vec<_>>()
            .join(" ");
    }
    Command::new("/usr/sbin/sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_else(|_| "unavailable".to_string())
}
