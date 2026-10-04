//! Complete sidecars and per-instance observation under uploads/root ownership.
use super::*;

pub(crate) const MAX_IDLE_SECS: u64 = 21_600;
pub(crate) type Observations = HashMap<UploadId, (Uuid, u64, std::time::Instant)>;

/// Wall timestamp is Unix milliseconds; bounded reads and serde reject bad shape.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Lease {
    pub(super) owner: Uuid,
    pub(super) idle_secs: u64,
    pub(super) renew_seq: u64,
    pub(super) renewed_at: u64,
}

impl Lease {
    pub(super) fn begin(config: UploadLeaseConfig) -> StorageResult<Self> {
        let value = Self {
            owner: config.owner(),
            idle_secs: config.idle_secs(),
            renew_seq: 0,
            renewed_at: timestamp()?,
        };
        value.validate()?;
        Ok(value)
    }
    fn validate(&self) -> StorageResult<()> {
        if self.idle_secs == 0
            || self.idle_secs > MAX_IDLE_SECS
            || SystemTime::UNIX_EPOCH
                .checked_add(Duration::from_millis(self.renewed_at))
                .is_none()
        {
            return Err(invalid(
                "upload_lease",
                "invalid upload lease bound or timestamp",
            ));
        }
        Ok(())
    }
    pub(super) fn next(mut self) -> StorageResult<Self> {
        self.renew_seq = self
            .renew_seq
            .checked_add(1)
            .ok_or_else(|| invalid("renew_upload", "upload lease sequence exhausted"))?;
        self.renewed_at = timestamp()?;
        Ok(self)
    }
}

fn timestamp() -> StorageResult<u64> {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .and_then(|value| u64::try_from(value.as_millis()).ok())
        .ok_or_else(|| invalid("upload_lease", "wall timestamp cannot be represented"))
}

pub(super) fn step<T>(
    context: &UploadContext,
    operation: &'static str,
    action: impl FnOnce() -> std::io::Result<T>,
) -> StorageResult<T> {
    #[cfg(unix)]
    {
        context.publication.step(operation, action)
    }
    #[cfg(not(unix))]
    {
        let _ = context;
        action().map_err(|error| map_io_err(error, operation))
    }
}

pub(super) fn open(directory: &UploadDirectory, name: &str) -> std::io::Result<fs::File> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        openat_regular_file_no_follow(directory.as_raw_fd(), name, libc::O_RDONLY)
    }
    #[cfg(not(unix))]
    {
        let path = directory.join(name);
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "upload entry must be regular",
            ));
        }
        fs::File::open(path)
    }
}

pub(super) fn unlink(directory: &UploadDirectory, name: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        unlink_entry_at(directory.as_raw_fd(), name)
    }
    #[cfg(not(unix))]
    {
        fs::remove_file(directory.join(name))
    }
}

pub(super) fn read(directory: &UploadDirectory, id: &UploadId) -> StorageResult<Option<Lease>> {
    let file = match open(directory, &format!("{id}.lease")) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(map_io_err(error, "upload_lease_open")),
    };
    let mut bytes = Vec::new();
    file.take(1025)
        .read_to_end(&mut bytes)
        .map_err(|error| map_io_err(error, "upload_lease_read"))?;
    if bytes.len() > 1024 {
        return Err(invalid("upload_lease", "oversized upload lease"));
    }
    let value: Lease = serde_json::from_slice(&bytes)
        .map_err(|error| invalid("upload_lease", error.to_string()))?;
    value.validate()?;
    Ok(Some(value))
}

pub(super) fn publish(
    context: &UploadContext,
    directory: &UploadDirectory,
    id: &UploadId,
    value: &Lease,
) -> StorageResult<()> {
    value.validate()?;
    let bytes =
        serde_json::to_vec(value).map_err(|error| invalid("upload_lease", error.to_string()))?;
    context.check_space(bytes.len() as u64)?;
    let temp = format!(".{id}.lease-{}", Uuid::new_v4());
    let result = (|| {
        #[cfg(unix)]
        let file = {
            use std::os::fd::AsRawFd;
            create_regular_file_at_no_follow(directory.as_raw_fd(), &temp, 0o600)
        };
        #[cfg(not(unix))]
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(directory.join(&temp));
        let mut file = file.map_err(|error| map_io_err(error, "lease_create"))?;
        file.write_all(&bytes)
            .map_err(|error| map_io_err(error, "lease_write"))?;
        step(context, "lease_sync_file", || file.sync_all())?;
        drop(file);
        // Close the file before replacement on platforms that restrict open renames.
        #[cfg(unix)]
        let rename = || {
            use std::os::fd::AsRawFd;
            rename_entry_at(
                directory.as_raw_fd(),
                &temp,
                directory.as_raw_fd(),
                &format!("{id}.lease"),
            )
        };
        #[cfg(not(unix))]
        let rename = || fs::rename(directory.join(&temp), directory.join(format!("{id}.lease")));
        step(context, "lease_replace", rename)?;
        #[cfg(unix)]
        context
            .publication
            .sync_directory("lease_sync_directory", directory)
            .map_err(|error| map_io_err(error, "lease_sync_directory"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = unlink(directory, &temp);
    }
    result
}

/// Called only after publication/unlink proves stage absence. Cleanup may retry.
pub(super) fn cleanup(context: &UploadContext, id: &UploadId) -> StorageResult<()> {
    context
        .observations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(id);
    let directory = match context.directory(false) {
        Ok(directory) => directory,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(map_io_err(error, "lease_cleanup_open")),
    };
    let name = format!("{id}.lease");
    match open(&directory, &name) {
        Ok(file) => {
            drop(file);
            step(context, "lease_unlink", || unlink(&directory, &name))?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(map_io_err(error, "lease_cleanup_open")),
    }
    // A missing entry cannot discharge a previous failed cleanup barrier.
    #[cfg(unix)]
    context
        .publication
        .sync_directory("lease_sync_cleanup", &directory)
        .map_err(|error| map_io_err(error, "lease_sync_cleanup"))?;
    Ok(())
}

pub(super) fn expired(
    context: &UploadContext,
    id: &UploadId,
    value: &Lease,
    wall: SystemTime,
    monotonic: std::time::Instant,
) -> StorageResult<bool> {
    let renewed = SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_millis(value.renewed_at))
        .ok_or_else(|| invalid("upload_lease", "unrepresentable renewal timestamp"))?;
    if renewed
        .duration_since(wall)
        .is_ok_and(|ahead| ahead > Duration::from_secs(300))
    {
        tracing::warn!(upload_id = %id, renewed_at = value.renewed_at,
            "upload lease clock fault: renewal exceeds five-minute future tolerance");
    }
    let threshold = Duration::from_secs(
        value
            .idle_secs
            .checked_add(300)
            .ok_or_else(|| invalid("upload_lease", "unrepresentable observation bound"))?,
    );
    let backstop = renewed
        .checked_add(Duration::from_secs(value.idle_secs))
        .and_then(|time| time.checked_add(Duration::from_secs(86_400)))
        .ok_or_else(|| invalid("upload_lease", "unrepresentable lease backstop"))?;
    let mut observations = context
        .observations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let observed =
        observations
            .entry(id.clone())
            .or_insert((value.owner, value.renew_seq, monotonic));
    if (observed.0, observed.1) != (value.owner, value.renew_seq) {
        *observed = (value.owner, value.renew_seq, monotonic);
    }
    Ok(monotonic
        .checked_duration_since(observed.2)
        .is_some_and(|age| age >= threshold)
        || wall >= backstop)
}
