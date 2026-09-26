use std::fmt;
#[cfg(unix)]
use std::process::Stdio;
#[cfg(unix)]
use std::time::Duration;

use khive_runtime::engine_config::{GitWriteActorConfig, GitWriteSectionConfig};
#[cfg(any(unix, test))]
use tokio::io::{AsyncRead, AsyncReadExt};
#[cfg(unix)]
use tokio::process::{Child, Command};
#[cfg(any(unix, test))]
use zeroize::Zeroizing;

#[cfg(unix)]
const RESOLVER_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(any(unix, test))]
const MAX_CREDENTIAL_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CredentialError;

impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("actor_unmapped")
    }
}

impl std::error::Error for CredentialError {}

/// The refusal payload an `actor_unmapped` receipt carries (ADR-182 Amendment 10).
/// `table` and `key` name what was consulted, not what was missing, because the
/// reason covers two states: no row for the label, and a row whose resolution
/// failed. Membership in the table is the only question that separates them, so it
/// is asked here rather than carried out of the resolver.
pub(crate) fn actor_refusal(config: &GitWriteSectionConfig, actor: &str) -> serde_json::Value {
    serde_json::json!({"refusal": {
        "table": "git_write.actors",
        "key": actor,
        "cause": if config.actors.contains_key(actor) { "resolver" } else { "absent" },
    }})
}

pub(crate) async fn resolve_actor(
    config: &GitWriteSectionConfig,
    actor: &str,
) -> Result<GitWriteActorConfig, CredentialError> {
    let identity = config.actors.get(actor).ok_or(CredentialError)?;
    config.validate_dev_loop().map_err(|_| CredentialError)?;
    #[cfg(unix)]
    {
        let secret = resolve_reference(config, &identity.credential_ref).await?;
        // Local object writes need the actor identity, but never the resolved token.
        drop(secret);
        Ok(identity.clone())
    }
    #[cfg(not(unix))]
    {
        // No process-tree containment is implemented on this platform yet.
        // Refuse before spawn instead of leaving resolver descendants running.
        let _ = identity;
        Err(CredentialError)
    }
}

#[cfg(any(unix, test))]
pub(crate) struct Secret {
    bytes: Zeroizing<Box<[u8]>>,
    len: usize,
}

#[cfg(any(unix, test))]
impl Secret {
    pub(crate) fn value(&self) -> &str {
        std::str::from_utf8(&self.bytes[..self.len]).expect("validated resolver UTF-8")
    }
}

#[cfg(unix)]
pub(crate) async fn resolve_remote(
    config: &GitWriteSectionConfig,
    actor: &str,
) -> Result<(GitWriteActorConfig, Secret), CredentialError> {
    config.validate_dev_loop().map_err(|_| {
        #[cfg(test)]
        eprintln!("remote credential diagnostic: config_validation");
        CredentialError
    })?;
    let Some(identity) = config.actors.get(actor) else {
        #[cfg(test)]
        eprintln!("remote credential diagnostic: actor_lookup");
        return Err(CredentialError);
    };
    let identity = identity.clone();
    let secret = resolve_reference(config, &identity.credential_ref).await?;
    Ok((identity, secret))
}

#[cfg(any(unix, test))]
async fn read_secret(reader: &mut (impl AsyncRead + Unpin)) -> Result<Secret, CredentialError> {
    // A fixed allocation prevents reallocations from leaving secret copies behind.
    let mut secret = Secret {
        bytes: Zeroizing::new(vec![0_u8; MAX_CREDENTIAL_BYTES + 1].into_boxed_slice()),
        len: 0,
    };
    loop {
        let read = reader
            .read(&mut secret.bytes[secret.len..])
            .await
            .map_err(|_| CredentialError)?;
        if read == 0 {
            break;
        }
        secret.len += read;
        if secret.len > MAX_CREDENTIAL_BYTES {
            return Err(CredentialError);
        }
    }
    if secret.len > 0 && secret.bytes[secret.len - 1] == b'\n' {
        secret.len -= 1;
        if secret.len > 0 && secret.bytes[secret.len - 1] == b'\r' {
            secret.len -= 1;
        }
    }
    let value = &secret.bytes[..secret.len];
    if value.is_empty()
        || value.iter().any(|byte| matches!(*byte, 0 | b'\r' | b'\n'))
        || std::str::from_utf8(value).is_err()
    {
        return Err(CredentialError);
    }
    Ok(secret)
}

#[cfg(unix)]
struct ResolverChild {
    child: Option<Child>,
    // Captured before any wait operation. WNOWAIT pins this leader (and its
    // process-group id) until the group has been signalled.
    pgid: libc::pid_t,
}

#[cfg(unix)]
fn signal_group(pgid: libc::pid_t, signal: libc::c_int) -> std::io::Result<()> {
    debug_assert!(pgid > 1);
    // SAFETY: pgid is the resolver's own process-group id, captured at spawn.
    if unsafe { libc::kill(-pgid, signal) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn darwin_group_contains_only_unreaped_leader(pgid: libc::pid_t) -> bool {
    // XNU's group-signal iterator excludes zombies, so a group containing
    // only our WNOWAIT leader returns EPERM for both SIGKILL and signal 0.
    // libproc lists both live and zombie group members. A one-PID result is
    // complete (the kernel's list cap is larger); zero can also mean an
    // enumeration error, and any other member could still be running.
    let mut members = [0 as libc::pid_t; 64];
    // SAFETY: the buffer is writable and its byte length fits c_int.
    let count = unsafe {
        libc::proc_listpgrppids(
            pgid,
            members.as_mut_ptr().cast(),
            libc::c_int::try_from(std::mem::size_of_val(&members))
                .expect("fixed process-list buffer fits c_int"),
        )
    };
    #[cfg(test)]
    eprintln!("remote credential diagnostic: group_members count={count}");
    count == 1 && members[0] == pgid
}

#[cfg(unix)]
fn confirmed_group_cleanup(
    signal: std::io::Result<()>,
    probe: impl FnOnce() -> std::io::Result<()>,
    zombie_only: impl FnOnce() -> bool,
) -> Result<(), CredentialError> {
    #[cfg(not(target_os = "macos"))]
    let _ = zombie_only;
    match signal {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
        Err(signal_error) => {
            #[cfg(not(target_os = "macos"))]
            let _ = &signal_error;
            let Err(probe_error) = probe() else {
                return Err(CredentialError);
            };
            // A failed SIGKILL is harmless only if a fresh, signal-free
            // existence probe confirms that the whole group is gone.
            if probe_error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(());
            }
            // Darwin also returns EPERM for a group containing only its
            // unreaped zombie leader. Require an authoritative member list;
            // a live or inaccessible descendant still refuses.
            #[cfg(target_os = "macos")]
            if signal_error.raw_os_error() == Some(libc::EPERM)
                && probe_error.raw_os_error() == Some(libc::EPERM)
                && zombie_only()
            {
                return Ok(());
            }
            Err(CredentialError)
        }
    }
}

#[cfg(unix)]
impl ResolverChild {
    fn child(&mut self) -> &mut Child {
        self.child
            .as_mut()
            .expect("resolver owns its child until drop")
    }

    /// Keep the leader unreaped until the whole process group has been
    /// signalled. Reaping first would allow its PID (also the group ID) to be
    /// reused before the signal reaches background descendants.
    fn kill_group(&self) -> Result<(), CredentialError> {
        // The guard still owns the unreaped process-group leader.
        let signal = signal_group(self.pgid, libc::SIGKILL);
        #[cfg(test)]
        if let Err(error) = &signal {
            eprintln!(
                "remote credential diagnostic: group_signal errno={:?}",
                error.raw_os_error()
            );
        }
        confirmed_group_cleanup(
            signal,
            || {
                let probe = signal_group(self.pgid, 0);
                #[cfg(test)]
                eprintln!(
                    "remote credential diagnostic: group_probe errno={:?}",
                    probe.as_ref().err().and_then(std::io::Error::raw_os_error)
                );
                probe
            },
            || {
                #[cfg(target_os = "macos")]
                {
                    darwin_group_contains_only_unreaped_leader(self.pgid)
                }
                #[cfg(not(target_os = "macos"))]
                {
                    false
                }
            },
        )
    }
}

#[cfg(unix)]
async fn wait_exited_without_reaping(pid: u32) -> Result<(), CredentialError> {
    loop {
        // `waitid(WNOWAIT)` observes the exit code but leaves the leader as a
        // child. The guard can still safely signal its process group.
        // Keep siginfo_t, which is not Send on all targets, out of the
        // future's state across the sleep below.
        let exited = {
            // SAFETY: zeroed siginfo_t is valid output storage for waitid.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: pid is the child owned by ResolverChild; waitid writes info.
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
                )
            };
            if result == 0 {
                info.si_signo == libc::SIGCHLD
            } else if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                #[cfg(test)]
                eprintln!(
                    "remote credential diagnostic: waitid errno={:?}",
                    std::io::Error::last_os_error().raw_os_error()
                );
                return Err(CredentialError);
            } else {
                false
            }
        };
        if exited {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[cfg(unix)]
impl Drop for ResolverChild {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if child.id() != Some(self.pgid as u32) {
            return;
        }
        // SAFETY: this unreaped child was spawned as its own process group.
        // Its PID cannot be reused while we hold the unreaped child.
        let _ = signal_group(self.pgid, libc::SIGKILL);
        let _ = child.start_kill();
        // Drop also runs when the calling future is cancelled. Reap independently
        // after termination instead of leaving cleanup tied to that cancelled future.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let _reaper = runtime.spawn(async move {
                let _ = child.wait().await;
            });
        }
    }
}

#[cfg(unix)]
async fn resolve_reference(
    config: &GitWriteSectionConfig,
    reference: &str,
) -> Result<Secret, CredentialError> {
    let mut command = Command::new(&config.credential_resolver[0]);
    command
        .args(config.credential_resolver[1..].iter().map(|arg| {
            if arg == "{ref}" {
                reference
            } else {
                arg.as_str()
            }
        }))
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(home) = std::env::var_os("HOME") {
        command.env("HOME", home);
    }
    command.current_dir("/").process_group(0);

    let child = command.spawn().map_err(|_error| {
        #[cfg(test)]
        eprintln!(
            "remote credential diagnostic: spawn errno={:?}",
            _error.raw_os_error()
        );
        CredentialError
    })?;
    let pgid = child
        .id()
        .and_then(|pid| libc::pid_t::try_from(pid).ok())
        .filter(|pid| *pid > 1)
        .ok_or(CredentialError)?;
    let mut child = ResolverChild {
        child: Some(child),
        pgid,
    };
    let mut stdout = child.child().stdout.take().ok_or(CredentialError)?;
    tokio::time::timeout(RESOLVER_TIMEOUT, async {
        let secret = read_secret(&mut stdout).await.map_err(|_| {
            #[cfg(test)]
            eprintln!("remote credential diagnostic: read_secret");
            CredentialError
        })?;
        wait_exited_without_reaping(child.pgid as u32).await?;
        child.kill_group().map_err(|_| {
            #[cfg(test)]
            eprintln!("remote credential diagnostic: kill_group");
            CredentialError
        })?;
        let status = child.child().wait().await.map_err(|_error| {
            #[cfg(test)]
            eprintln!(
                "remote credential diagnostic: reap errno={:?}",
                _error.raw_os_error()
            );
            CredentialError
        })?;
        if !status.success() {
            #[cfg(test)]
            eprintln!("remote credential diagnostic: nonzero_exit={status}");
            return Err(CredentialError);
        }
        Ok(secret)
    })
    .await
    .map_err(|_| CredentialError)?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unmapped_actor_refuses_without_a_resolver() {
        let config = GitWriteSectionConfig {
            credential_resolver: vec!["/does/not/exist".to_string(), "{ref}".to_string()],
            ..Default::default()
        };
        let error = resolve_actor(&config, "unmapped").await.unwrap_err();
        assert_eq!(error.to_string(), "actor_unmapped");
        assert_eq!(format!("{error:?}"), "CredentialError");
    }

    #[tokio::test]
    async fn invalid_programmatic_configuration_refuses_before_spawn() {
        let mut config = GitWriteSectionConfig {
            credential_resolver: vec![],
            ..Default::default()
        };
        config.actors.insert(
            "configured".to_string(),
            GitWriteActorConfig {
                name: "Example".to_string(),
                email: "example@example.invalid".to_string(),
                credential_ref: "example-reference".to_string(),
                platform_identity: "example-login".to_string(),
            },
        );
        assert!(resolve_actor(&config, "configured").await.is_err());
    }

    #[tokio::test]
    async fn resolver_output_preserves_value_and_removes_one_line_ending() {
        for output in [
            b"synthetic-value".as_slice(),
            b"synthetic-value\n",
            b"synthetic-value\r\n",
        ] {
            let mut reader = output;
            let secret = read_secret(&mut reader).await.ok().unwrap();
            assert_eq!(&secret.bytes[..secret.len], b"synthetic-value");
        }
    }

    #[tokio::test]
    async fn malformed_resolver_output_refuses() {
        for output in [b"".as_slice(), b"\n", b"value\nsecond", b"value\0", b"\xff"] {
            let mut reader = output;
            assert!(read_secret(&mut reader).await.is_err());
        }
    }

    #[tokio::test]
    async fn resolver_output_is_bounded() {
        let at_limit = vec![b'x'; MAX_CREDENTIAL_BYTES];
        assert!(read_secret(&mut at_limit.as_slice()).await.is_ok());
        let above_limit = vec![b'x'; MAX_CREDENTIAL_BYTES + 1];
        assert!(read_secret(&mut above_limit.as_slice()).await.is_err());
    }

    #[cfg(unix)]
    #[test]
    fn failed_group_signal_requires_a_confirmed_absent_group() {
        let denied = || std::io::Error::from_raw_os_error(libc::EPERM);
        let absent = || std::io::Error::from_raw_os_error(libc::ESRCH);
        assert!(confirmed_group_cleanup(
            Ok(()),
            || panic!("unexpected probe"),
            || panic!("unexpected group census")
        )
        .is_ok());
        assert!(confirmed_group_cleanup(
            Err(absent()),
            || panic!("unexpected probe"),
            || panic!("unexpected group census")
        )
        .is_ok());
        assert!(confirmed_group_cleanup(Err(denied()), || Err(absent()), || false).is_ok());
        assert!(confirmed_group_cleanup(Err(denied()), || Ok(()), || true).is_err());
        assert!(confirmed_group_cleanup(Err(denied()), || Err(denied()), || false).is_err());
        #[cfg(target_os = "macos")]
        {
            assert!(confirmed_group_cleanup(Err(denied()), || Err(denied()), || true).is_ok());
            assert!(confirmed_group_cleanup(
                Err(std::io::Error::from_raw_os_error(libc::EINVAL)),
                || Err(denied()),
                || true
            )
            .is_err());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolver_without_descendants_preserves_a_mapped_actor() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let program = tmp.path().join("resolver-test");
        let reference = tmp.path().join("reference");
        std::fs::write(&reference, "synthetic-value").unwrap();
        std::fs::write(&program, "#!/bin/sh\nexec /bin/cat \"$1\"\n").unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut config = GitWriteSectionConfig {
            credential_resolver: vec![program.display().to_string(), "{ref}".to_string()],
            ..Default::default()
        };
        config.actors.insert(
            "mapped".to_string(),
            GitWriteActorConfig {
                name: "Example".to_string(),
                email: "example@example.invalid".to_string(),
                credential_ref: reference.display().to_string(),
                platform_identity: "example-login".to_string(),
            },
        );
        let (_, secret) = resolve_remote(&config, "mapped").await.unwrap();
        assert_eq!(secret.value(), "synthetic-value");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn resolver_reaps_background_descendants_after_normal_exits() {
        use std::os::unix::fs::PermissionsExt;

        for exit_code in [0, 7] {
            let tmp = tempfile::TempDir::new().unwrap();
            let program = tmp.path().join("resolver-test");
            let marker = tmp.path().join("background-ran");
            std::fs::write(
                &program,
                format!(
                    "#!/bin/sh\n(sleep 2; printf ran > \"$1\") </dev/null >/dev/null 2>&1 &\nprintf 'fake-value\\n'\nexit {exit_code}\n"
                ),
            )
            .unwrap();
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
            let config = GitWriteSectionConfig {
                credential_resolver: vec![program.display().to_string(), "{ref}".to_string()],
                ..Default::default()
            };
            let result = resolve_reference(&config, marker.to_str().unwrap()).await;
            assert_eq!(result.is_ok(), exit_code == 0);
            tokio::time::sleep(Duration::from_millis(2300)).await;
            assert!(
                !marker.exists(),
                "resolver left a running descendant after exit {exit_code}"
            );
        }
    }
}
