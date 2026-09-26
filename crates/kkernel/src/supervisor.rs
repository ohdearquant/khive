//! Launcher-owned daemon declarations (ADR-185 Amendment 1).

use std::ffi::OsString;

use anyhow::Result;
use clap::{Args, Subcommand};

#[derive(Debug, Subcommand)]
pub(crate) enum SupervisorCommand {
    /// Resolve configuration, declare ownership, then exec this binary as a daemon.
    Launch(LaunchArgs),
    /// Release this label's declaration after deliberately stopping its supervisor job.
    Release(ReleaseArgs),
}

#[derive(Debug, Args)]
pub(crate) struct LaunchArgs {
    /// Supervisor job label, written as the first marker line.
    #[arg(long)]
    label: String,

    /// Supervisor restart interval in positive whole seconds.
    #[arg(long, value_name = "SECONDS")]
    restart_interval_secs: String,

    /// Existing `mcp` options, excluding the `mcp` command and `--daemon` flag.
    #[arg(last = true, allow_hyphen_values = true, value_name = "MCP_ARG")]
    mcp_args: Vec<OsString>,
}

#[derive(Debug, Args)]
pub(crate) struct ReleaseArgs {
    #[arg(long)]
    label: String,
}

#[cfg(unix)]
pub(crate) async fn run(command: SupervisorCommand, log: &str) -> Result<()> {
    let marker = khive_runtime::daemon::supervisor_marker_path();
    match command {
        SupervisorCommand::Launch(args) => unix::launch(args, log, &marker).await,
        SupervisorCommand::Release(args) => {
            unix::validate_label(&args.label)?;
            unix::MarkerGuard::acquire(&marker)?.release(&args.label)
        }
    }
}

#[cfg(not(unix))]
pub(crate) async fn run(_command: SupervisorCommand, _log: &str) -> Result<()> {
    anyhow::bail!("supervisor launch/release requires Unix (same-process daemon exec)")
}

#[cfg(unix)]
mod unix {
    use std::fs::{self, File, OpenOptions};
    use std::io::Write;
    use std::os::unix::{fs::OpenOptionsExt, process::CommandExt};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::{Duration, Instant};

    use anyhow::{bail, Context, Result};
    use clap::Parser;
    use khive_mcp::daemon::{
        probe_supervisor_socket, supervisor_effective_uid, supervisor_pid_is_alive,
        supervisor_sigterm, SupervisorSocketProbe,
    };
    use khive_mcp::serve::{
        config_discovery_db_anchor, reject_conflicting_db_override_with_source,
        resolve_runtime_config_with_db_anchor, RuntimeConfigInputs,
    };
    use khive_runtime::KhiveConfig;

    use super::LaunchArgs;

    pub(super) fn validate_label(label: &str) -> Result<()> {
        if label.is_empty() || label.trim() != label || label.chars().any(char::is_control) {
            bail!("supervisor label must be a nonempty single line without surrounding whitespace");
        }
        Ok(())
    }

    fn interval_seconds(raw: &str) -> Result<u64> {
        // Leave room for the client's three-interval wait bound, without
        // silently accepting fractional, signed, zero, or overflowing input.
        if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
            bail!("restart interval must be positive whole seconds");
        }
        let seconds: u64 = raw.parse().context("restart interval is too large")?;
        if seconds == 0 || seconds.checked_mul(3).is_none() {
            bail!("restart interval must be positive and fit a three-interval wait bound");
        }
        Ok(seconds)
    }

    pub(super) struct MarkerGuard {
        path: PathBuf,
        // Keep the inode forever: deleting a lock file can split concurrent
        // publishers onto different locks. Rust opens this fd close-on-exec.
        _lock: File,
    }

    impl MarkerGuard {
        pub(super) fn acquire(path: &Path) -> Result<Self> {
            let parent = path.parent().filter(|path| !path.as_os_str().is_empty());
            if let Some(parent) = parent {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create marker directory {}", parent.display()))?;
            }
            let mut lock_path = path.as_os_str().to_os_string();
            lock_path.push(".lock");
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(PathBuf::from(lock_path))
                .context("open supervisor marker ownership lock")?;
            lock.lock().context("lock supervisor marker ownership")?;
            Ok(Self {
                path: path.to_path_buf(),
                _lock: lock,
            })
        }

        fn read_marker(&self) -> Result<Option<String>> {
            match fs::symlink_metadata(&self.path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error).context("inspect supervisor marker"),
                Ok(metadata) if !metadata.file_type().is_file() => {
                    bail!(
                        "supervisor marker is not a regular file; refusing to replace or remove it"
                    )
                }
                Ok(_) => {}
            }
            let body = fs::read_to_string(&self.path).context("read supervisor marker owner")?;
            Ok(Some(body))
        }

        fn owner(&self) -> Result<Option<String>> {
            // Only the label confers ownership. A prior launcher can be dead,
            // and a legacy two-line marker is still that label's declaration.
            Ok(self
                .read_marker()?
                .map(|body| body.lines().next().unwrap_or_default().to_owned()))
        }

        fn require_owner(&self, label: &str) -> Result<bool> {
            match self.owner()? {
                None => Ok(false),
                Some(owner) if owner == label => Ok(true),
                Some(owner) => bail!(
                    "supervisor marker {} belongs to {owner:?}, not {label:?}",
                    self.path.display()
                ),
            }
        }

        fn publish(&self, label: &str, interval: u64) -> Result<()> {
            self.require_owner(label)?;
            let parent = self
                .path
                .parent()
                .filter(|path| !path.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            let mut temporary = tempfile::NamedTempFile::new_in(parent)
                .context("create supervisor marker temporary file")?;
            write!(temporary, "{label}\n{}\n{interval}\n", std::process::id())
                .context("write supervisor marker")?;
            temporary
                .persist(&self.path)
                .context("atomically publish supervisor marker")?;
            Ok(())
        }

        pub(super) fn release(&self, label: &str) -> Result<()> {
            if self.require_owner(label)? {
                fs::remove_file(&self.path).context("remove owned supervisor marker")?;
            }
            Ok(())
        }
    }

    fn resolve_configuration(args: &khive_mcp::args::Args) -> Result<()> {
        let discovery_anchor = config_discovery_db_anchor(args.db.as_deref());
        let loaded = KhiveConfig::load_with_home_fallback_and_source(
            args.config.as_deref(),
            discovery_anchor.as_deref(),
        )?;
        if let Some((config, source)) = &loaded {
            reject_conflicting_db_override_with_source(
                args.db.as_deref(),
                &config.backends,
                Some(source),
            )?;
        }
        let (explicit, namespace) =
            khive_mcp::args::resolve_cli_namespace(args).map_err(anyhow::Error::msg)?;
        let (resolved, _) = resolve_runtime_config_with_db_anchor(RuntimeConfigInputs {
            db: args.db.as_deref(),
            config: args.config.as_deref(),
            namespace,
            namespace_explicit: explicit,
            actor_explicit: explicit,
            no_embed: args.no_embed,
            packs: (!args.pack.is_empty()).then(|| args.pack.clone()),
            brain_profile: args.brain_profile.clone(),
        })?;
        khive_runtime::PackRegistry::validate_pack_selection(&resolved.packs)?;
        // Resolution itself opens no database, migration, model, transport,
        // or incumbent-daemon probe. The launcher's separate own-job probe
        // may already have run while holding the marker lock.
        Ok(())
    }

    pub(super) async fn launch(args: LaunchArgs, log: &str, marker: &Path) -> Result<()> {
        if let Err(error) = validate_label(&args.label) {
            eprintln!("CONFIG: {error:#}");
            return Ok(());
        }
        let guard = MarkerGuard::acquire(marker)?;
        let prior_marker = guard.read_marker()?;
        if let Some(owner) = prior_marker
            .as_deref()
            .map(|body| body.lines().next().unwrap_or_default())
        {
            if owner != args.label {
                eprintln!(
                    "CONFIG: supervisor marker {} belongs to {owner:?}, not {:?}; refusing launch",
                    marker.display(),
                    args.label
                );
                return Ok(());
            }
        }
        // A second launch of an already-serving job must leave its declaration
        // byte-identical, even if the second launch supplied bad configuration.
        // A stale/reused marker PID alone does not prove this: the same PID
        // must answer on the socket as a daemon.
        if let Some(prior_pid) = prior_marker
            .as_deref()
            .and_then(|body| body.lines().nth(1))
            .and_then(|pid| pid.parse::<u32>().ok())
            .filter(|pid| *pid > 0)
        {
            match probe_supervisor_socket(Duration::from_millis(500)).await {
                SupervisorSocketProbe::Daemon(peer) if peer.pid == Some(prior_pid) => {
                    bail!(
                        "duplicate supervisor launch for {:?}: job pid {prior_pid} already serves the socket",
                        args.label
                    );
                }
                SupervisorSocketProbe::Unidentified { pid, uid, reason } => {
                    bail!(
                        "own-job incumbent could not be identified; preserving marker: marker_pid={prior_pid} peer_pid={pid:?} uid={uid:?}: {reason}"
                    );
                }
                SupervisorSocketProbe::Daemon(peer) if peer.pid.is_none() => {
                    bail!(
                        "own-job incumbent has no peer pid; preserving marker: marker_pid={prior_pid} uid={:?}",
                        peer.uid
                    );
                }
                SupervisorSocketProbe::Absent | SupervisorSocketProbe::Daemon(_) => {}
            }
        }
        let prepared = (|| -> Result<u64> {
            let interval = interval_seconds(&args.restart_interval_secs)?;
            let mcp_args = khive_mcp::args::Args::try_parse_from(
                ["mcp".into(), "--daemon".into()]
                    .into_iter()
                    .chain(args.mcp_args.iter().cloned()),
            )?;
            resolve_configuration(&mcp_args)?;
            Ok(interval)
        })();
        let interval = match prepared {
            Ok(interval) => interval,
            Err(error) => {
                guard.release(&args.label)?;
                eprintln!("CONFIG: supervisor {:?}: {error:#}", args.label);
                return Ok(());
            }
        };
        let executable = std::env::current_exe().context("resolve current kkernel executable")?;
        guard.publish(&args.label, interval)?;
        // The client may have won the marker lock first and started an
        // unmanaged daemon. The launcher owns the lock through this handover
        // and exec, so no new client can race into the old socket afterwards.
        let probe_started = Instant::now();
        match probe_supervisor_socket(Duration::from_millis(500)).await {
            SupervisorSocketProbe::Absent => {}
            SupervisorSocketProbe::Unidentified { pid, uid, reason } => {
                bail!(
                    "incumbent is not a khive daemon: pid={pid:?} uid={uid:?} waited={:?}: {reason}",
                    probe_started.elapsed()
                );
            }
            SupervisorSocketProbe::Daemon(peer) => {
                let pid = peer.pid;
                let uid = peer.uid;
                let Some(incumbent_pid) = pid.filter(|pid| *pid != std::process::id()) else {
                    bail!(
                        "incumbent did not yield: pid={pid:?} uid={uid:?} waited={:?}: no usable peer pid",
                        probe_started.elapsed()
                    );
                };
                if uid != Some(supervisor_effective_uid()) {
                    bail!(
                        "incumbent did not yield: pid={pid:?} uid={uid:?} waited={:?}: foreign uid",
                        probe_started.elapsed()
                    );
                }
                if let Err(error) = supervisor_sigterm(incumbent_pid) {
                    bail!(
                        "incumbent did not yield: pid={pid:?} uid={uid:?} waited={:?}: SIGTERM failed: {error}",
                        probe_started.elapsed()
                    );
                }
                let wait = Duration::from_secs(interval);
                let wait_started = Instant::now();
                loop {
                    let remaining = wait.saturating_sub(wait_started.elapsed());
                    let socket_absent = matches!(
                        probe_supervisor_socket(remaining.min(Duration::from_millis(200))).await,
                        SupervisorSocketProbe::Absent
                    );
                    if socket_absent && !supervisor_pid_is_alive(incumbent_pid) {
                        break;
                    }
                    if wait_started.elapsed() >= wait {
                        bail!(
                            "incumbent did not yield: pid={pid:?} uid={uid:?} waited={:?}",
                            wait_started.elapsed()
                        );
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                eprintln!(
                    "supervisor replaced client-started incumbent: pid={incumbent_pid} uid={} waited={:?}",
                    uid.unwrap_or_default(),
                    wait_started.elapsed()
                );
            }
        }
        let error = Command::new(executable)
            .args(["--log", log, "mcp", "--daemon"])
            .args(&args.mcp_args)
            .exec();
        // exec never returns on success. Keep serialization through exec so
        // a deliberate release cannot interleave between publication and exec.
        // An exec system error is a launch failure, never a CONFIG success.
        guard.release(&args.label)?;
        Err(error).context("exec supervised daemon")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn marker_has_three_lines_and_same_label_can_restart() {
            let root = tempfile::tempdir().unwrap();
            let marker = root.path().join("khived.supervisor");
            fs::write(&marker, "job\n123\n").unwrap();
            let guard = MarkerGuard::acquire(&marker).unwrap();
            guard.publish("job", 7).unwrap();
            assert_eq!(
                fs::read_to_string(&marker).unwrap(),
                format!("job\n{}\n7\n", std::process::id())
            );
            guard.publish("job", 11).unwrap();
            assert_eq!(
                fs::read_to_string(&marker).unwrap(),
                format!("job\n{}\n11\n", std::process::id())
            );
            guard.release("job").unwrap();
            assert!(!marker.exists());
            guard.release("job").unwrap();
            assert!(root.path().join("khived.supervisor.lock").exists());
        }

        #[test]
        fn foreign_claim_is_byte_protected_from_publish_and_release() {
            let root = tempfile::tempdir().unwrap();
            let marker = root.path().join("khived.supervisor");
            let foreign = b"other-job\n998\n30\n";
            fs::write(&marker, foreign).unwrap();
            let guard = MarkerGuard::acquire(&marker).unwrap();
            assert!(guard.publish("job", 10).is_err());
            assert_eq!(fs::read(&marker).unwrap(), foreign);
            assert!(guard.release("job").is_err());
            assert_eq!(fs::read(&marker).unwrap(), foreign);
        }

        #[test]
        fn publication_replaces_the_complete_file_and_preserves_old_open_reader() {
            use std::io::Read;
            let root = tempfile::tempdir().unwrap();
            let marker = root.path().join("khived.supervisor");
            let old = "job\n111\n5\n";
            fs::write(&marker, old).unwrap();
            let mut reader = File::open(&marker).unwrap();
            MarkerGuard::acquire(&marker)
                .unwrap()
                .publish("job", 10)
                .unwrap();
            let mut observed = String::new();
            reader.read_to_string(&mut observed).unwrap();
            assert_eq!(
                observed, old,
                "publication must rename, not truncate in place"
            );
            assert_eq!(
                fs::read_to_string(&marker).unwrap(),
                format!("job\n{}\n10\n", std::process::id())
            );
        }

        #[test]
        fn ownership_lock_serializes_competing_labels() {
            let root = tempfile::tempdir().unwrap();
            let marker = root.path().join("khived.supervisor");
            let guard = MarkerGuard::acquire(&marker).unwrap();
            let competing_lock = OpenOptions::new()
                .write(true)
                .open(root.path().join("khived.supervisor.lock"))
                .unwrap();
            assert!(matches!(
                competing_lock.try_lock(),
                Err(std::fs::TryLockError::WouldBlock)
            ));
            let contender_marker = marker.clone();
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let contender = std::thread::spawn(move || {
                started_tx.send(()).unwrap();
                let guard = MarkerGuard::acquire(&contender_marker).unwrap();
                done_tx.send(guard.publish("second", 20).is_err()).unwrap();
            });
            started_rx.recv().unwrap();
            guard.publish("first", 10).unwrap();
            drop(guard);
            assert!(done_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap());
            contender.join().unwrap();
            assert_eq!(
                fs::read_to_string(&marker).unwrap(),
                format!("first\n{}\n10\n", std::process::id())
            );
        }

        #[test]
        fn marker_symlink_is_never_a_claim_or_release_target() {
            let root = tempfile::tempdir().unwrap();
            let target = root.path().join("elsewhere");
            let marker = root.path().join("khived.supervisor");
            fs::write(&target, "job\n1\n10\n").unwrap();
            std::os::unix::fs::symlink(&target, &marker).unwrap();
            let guard = MarkerGuard::acquire(&marker).unwrap();
            assert!(guard.publish("job", 10).is_err());
            assert!(guard.release("job").is_err());
            assert!(fs::symlink_metadata(&marker)
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(fs::read_to_string(&target).unwrap(), "job\n1\n10\n");
        }

        #[test]
        fn declaration_fields_refuse_ambiguous_lines_or_invalid_intervals() {
            for label in ["", " job", "job ", "job\nother", "job\rother", "job\0"] {
                assert!(validate_label(label).is_err(), "{label:?}");
            }
            assert!(validate_label("ai.khive.kkernel-daemon").is_ok());
            for raw in ["", "0", "-1", "+1", "1.5", " 1", "18446744073709551615"] {
                assert!(interval_seconds(raw).is_err(), "{raw:?}");
            }
            assert_eq!(interval_seconds("10").unwrap(), 10);
        }

        #[tokio::test]
        async fn config_refusal_releases_only_the_launchers_own_label() {
            let root = tempfile::tempdir().unwrap();
            let marker = root.path().join("khived.supervisor");
            let args = || LaunchArgs {
                label: "job".into(),
                restart_interval_secs: "0".into(),
                mcp_args: vec![],
            };
            fs::write(&marker, "job\n123\n10\n").unwrap();
            launch(args(), "warn", &marker).await.unwrap();
            assert!(!marker.exists());

            let foreign = "other-job\n456\n10\n";
            fs::write(&marker, foreign).unwrap();
            launch(args(), "warn", &marker).await.unwrap();
            assert_eq!(fs::read_to_string(&marker).unwrap(), foreign);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct TestCli {
        #[command(subcommand)]
        command: SupervisorCommand,
    }

    #[test]
    fn supervisor_launch_preserves_the_mcp_argument_tail() {
        let parsed = TestCli::try_parse_from([
            "supervisor",
            "launch",
            "--label",
            "ai.khive.kkernel-daemon",
            "--restart-interval-secs",
            "10",
            "--",
            "--config",
            "some dir/khive.toml",
            "--pack",
            "kg",
            "--pack",
            "comm",
            "--actor",
            "test:fixture",
            "--no-embed",
        ])
        .unwrap();
        let SupervisorCommand::Launch(args) = parsed.command else {
            panic!("expected supervisor launch");
        };
        assert_eq!(args.label, "ai.khive.kkernel-daemon");
        assert_eq!(args.restart_interval_secs, "10");
        let expected: Vec<OsString> = [
            "--config",
            "some dir/khive.toml",
            "--pack",
            "kg",
            "--pack",
            "comm",
            "--actor",
            "test:fixture",
            "--no-embed",
        ]
        .into_iter()
        .map(OsString::from)
        .collect();
        assert_eq!(args.mcp_args, expected);
    }

    #[test]
    fn supervisor_release_accepts_a_label_without_a_claim_pid() {
        let parsed = TestCli::try_parse_from([
            "supervisor",
            "release",
            "--label",
            "ai.khive.kkernel-daemon",
        ])
        .unwrap();
        let SupervisorCommand::Release(args) = parsed.command else {
            panic!("expected supervisor release");
        };
        assert_eq!(args.label, "ai.khive.kkernel-daemon");
        assert!(TestCli::try_parse_from([
            "supervisor",
            "release",
            "--label",
            "job",
            "--pid",
            "123",
        ])
        .is_err());
    }
}
