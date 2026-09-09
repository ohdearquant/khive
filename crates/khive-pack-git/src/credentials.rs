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
struct Secret {
    bytes: Zeroizing<Box<[u8]>>,
    len: usize,
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
struct ResolverChild(Option<Child>);

#[cfg(unix)]
impl ResolverChild {
    fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("resolver owns its child until drop")
    }
}

#[cfg(unix)]
impl Drop for ResolverChild {
    fn drop(&mut self) {
        let Some(mut child) = self.0.take() else {
            return;
        };
        let Some(pid) = child.id() else {
            return;
        };
        // SAFETY: this unreaped child was spawned as its own process group.
        // Its PID cannot be reused while we hold the unreaped child.
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
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

    let child = command.spawn().map_err(|_| CredentialError)?;
    let mut child = ResolverChild(Some(child));
    let mut stdout = child.child().stdout.take().ok_or(CredentialError)?;
    tokio::time::timeout(RESOLVER_TIMEOUT, async {
        let secret = read_secret(&mut stdout).await?;
        let status = child.child().wait().await.map_err(|_| CredentialError)?;
        if !status.success() {
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
}
