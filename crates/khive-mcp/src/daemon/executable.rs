use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use rmcp::ErrorData as McpError;

#[derive(Clone, Debug, PartialEq, Eq)]
enum FileIdentity {
    Inode { device: u64, inode: u64 },
    Modified(SystemTime),
}

impl FileIdentity {
    fn from_metadata(metadata: Metadata) -> std::io::Result<Self> {
        if metadata.ino() != 0 {
            Ok(Self::Inode {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        } else {
            metadata.modified().map(Self::Modified)
        }
    }

    fn at(path: &Path) -> std::io::Result<Self> {
        Self::from_metadata(std::fs::metadata(path)?)
    }
}

#[derive(Clone)]
pub(crate) struct BridgeExecutable {
    path: PathBuf,
    identity: FileIdentity,
    last_check: Option<Instant>,
    replaced: bool,
}

static STARTUP_EXECUTABLE: std::sync::OnceLock<Option<BridgeExecutable>> =
    std::sync::OnceLock::new();

pub(super) fn capture_at_startup() {
    STARTUP_EXECUTABLE.get_or_init(|| {
        match std::env::current_exe().and_then(BridgeExecutable::at) {
            Ok(executable) => Some(executable),
            Err(error) => {
                tracing::debug!(%error, "bridge executable identity unavailable at startup");
                None
            }
        }
    });
}

impl BridgeExecutable {
    pub(crate) fn current() -> Option<Self> {
        capture_at_startup();
        STARTUP_EXECUTABLE.get().cloned().flatten()
    }

    pub(crate) fn at(path: PathBuf) -> std::io::Result<Self> {
        let path = path.canonicalize()?;
        Ok(Self {
            identity: FileIdentity::at(&path)?,
            path,
            last_check: None,
            replaced: false,
        })
    }

    pub(crate) fn check(&mut self) -> Result<(), McpError> {
        self.check_with(Instant::now(), FileIdentity::at)
    }

    fn check_with(
        &mut self,
        now: Instant,
        stat: impl FnOnce(&Path) -> std::io::Result<FileIdentity>,
    ) -> Result<(), McpError> {
        if !self.replaced
            && self
                .last_check
                .is_none_or(|last| now.saturating_duration_since(last) >= Duration::from_secs(1))
        {
            self.last_check = Some(now);
            match stat(&self.path) {
                Ok(identity) => self.replaced = identity != self.identity,
                Err(error) => {
                    tracing::debug!(%error, path = %self.path.display(), "bridge executable identity check unavailable")
                }
            }
        }
        if !self.replaced {
            return Ok(());
        }

        // Keep refusing admission while the response is waiting to flush. The
        // captured path also survives current_exe() gaining a " (deleted)"
        // suffix after an atomic install on Linux.
        super::arm_executable_self_heal(self.path.clone());
        Err(super::protocol_mismatch_error(
            "bridge executable replaced; the bridge re-execs the current binary after this response, retry the request".to_string(),
            Some(serde_json::json!({"reason": "executable_replaced"})),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::{fire_pending_self_heal, reset_self_heal_counters, REEXEC_INVOKED_COUNT};
    use std::sync::atomic::Ordering;

    fn replace(path: &Path) {
        let candidate = path.with_extension("next");
        std::fs::copy(path, &candidate).unwrap();
        std::fs::rename(candidate, path).unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn bridge_executable_current_image_does_not_reexec() {
        reset_self_heal_counters();
        let mut executable = BridgeExecutable::current().unwrap();
        assert_eq!(
            executable.identity,
            FileIdentity::at(&executable.path).unwrap()
        );
        executable.check().unwrap();
        fire_pending_self_heal();
        assert_eq!(REEXEC_INVOKED_COUNT.load(Ordering::SeqCst), 0);
    }

    #[test]
    #[serial_test::serial]
    fn bridge_executable_replacement_returns_retry_before_one_reexec() {
        reset_self_heal_counters();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bridge");
        std::fs::write(&path, b"binary image").unwrap();
        let mut executable = BridgeExecutable::at(path.clone()).unwrap();
        replace(&path);

        let error = executable
            .check()
            .expect_err("replacement must refuse the request");
        assert!(error.message.contains("retry the request"));
        let data = error.data.unwrap();
        assert_eq!(data["code"], "version_mismatch");
        assert_eq!(data["kind"], "protocol");
        assert_eq!(data["reason"], "executable_replaced");
        assert_eq!(REEXEC_INVOKED_COUNT.load(Ordering::SeqCst), 0);
        executable
            .check()
            .expect_err("no request admitted before the flush");
        fire_pending_self_heal();
        fire_pending_self_heal();
        assert_eq!(REEXEC_INVOKED_COUNT.load(Ordering::SeqCst), 1);

        let mut resumed = BridgeExecutable::at(path.clone()).unwrap();
        resumed.check().unwrap();
        replace(&path);
        resumed
            .check_with(Instant::now() + Duration::from_secs(1), FileIdentity::at)
            .expect_err("another install must be observed by the resumed image");
        fire_pending_self_heal();
        assert_eq!(REEXEC_INVOKED_COUNT.load(Ordering::SeqCst), 2);
    }

    #[test]
    #[serial_test::serial]
    fn bridge_executable_unchanged_requests_rate_limit_stat() {
        reset_self_heal_counters();
        let mut executable = BridgeExecutable::current().unwrap();
        let start = Instant::now();
        let mut calls = 0;
        for millis in 0..3000 {
            executable
                .check_with(start + Duration::from_millis(millis), |path| {
                    calls += 1;
                    FileIdentity::at(path)
                })
                .unwrap();
        }
        assert_eq!(calls, 3);
        fire_pending_self_heal();
        assert_eq!(REEXEC_INVOKED_COUNT.load(Ordering::SeqCst), 0);
    }

    #[test]
    #[serial_test::serial]
    fn bridge_executable_missing_or_unreadable_path_does_not_reexec() {
        reset_self_heal_counters();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bridge");
        std::fs::write(&path, b"binary image").unwrap();
        let mut executable = BridgeExecutable::at(path.clone()).unwrap();
        std::fs::remove_file(path).unwrap();
        executable.check().unwrap();
        executable
            .check_with(Instant::now() + Duration::from_secs(1), |_| {
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            })
            .unwrap();
        fire_pending_self_heal();
        assert_eq!(REEXEC_INVOKED_COUNT.load(Ordering::SeqCst), 0);
    }
}
