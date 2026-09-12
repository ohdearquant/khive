//! Process-local upload capabilities shared by the verbs and daemon sweeper.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use khive_runtime::{KhiveRuntime, RuntimeError};
use khive_storage::{BlobStore, ContentRef, UploadId};
use serde_json::{json, Value};

use crate::handlers::{blob_store, max_request_part_raw_bytes, MAX_OBJECT_BYTES};

#[derive(Clone, Copy)]
struct UploadPolicy {
    idle_for: Duration,
    sweep_interval: Duration,
}

impl UploadPolicy {
    fn from_env() -> Self {
        fn duration(name: &str, default: u64) -> Duration {
            match std::env::var(name) {
                Ok(raw) => match raw.parse::<u64>() {
                    Ok(seconds)
                        if seconds > 0
                            && Instant::now()
                                .checked_add(Duration::from_secs(seconds))
                                .is_some() =>
                    {
                        Duration::from_secs(seconds)
                    }
                    _ => {
                        tracing::warn!(name, "invalid upload duration; using default");
                        Duration::from_secs(default)
                    }
                },
                Err(_) => Duration::from_secs(default),
            }
        }
        Self {
            idle_for: duration("KHIVE_BLOB_UPLOAD_IDLE_SECS", 3600),
            sweep_interval: duration("KHIVE_BLOB_UPLOAD_SWEEP_INTERVAL_SECS", 600),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum UploadPhase {
    Active,
    InFlight,
    Aborted,
    Finished,
}

struct UploadRecord {
    store: Arc<dyn BlobStore>,
    size: u64,
    expected_ref: Option<ContentRef>,
    hasher: blake3::Hasher,
    received_bytes: u64,
    next_index: u64,
    tail: Option<(usize, blake3::Hash)>,
    actor: String,
    last_part: Instant,
    phase: UploadPhase,
}

/// Shared state owned by one loaded blob pack; upload ids do not survive restart.
pub struct UploadManager {
    runtime: KhiveRuntime,
    policy: UploadPolicy,
    records: Mutex<HashMap<UploadId, Arc<tokio::sync::Mutex<UploadRecord>>>>,
}

impl UploadManager {
    pub(crate) fn new(runtime: KhiveRuntime) -> Self {
        Self {
            runtime,
            policy: UploadPolicy::from_env(),
            records: Mutex::new(HashMap::new()),
        }
    }

    /// Delay between daemon-owned cleanup passes.
    pub fn sweep_interval(&self) -> Duration {
        self.policy.sweep_interval
    }

    fn write_store(&self) -> Result<Arc<dyn BlobStore>, RuntimeError> {
        if self.runtime.is_read_only() {
            return Err(RuntimeError::InvalidInput(
                "blob uploads are unavailable because the blob pack runtime is read-only".into(),
            ));
        }
        blob_store(&self.runtime)
    }

    fn unknown(id: &UploadId) -> RuntimeError {
        RuntimeError::NotFound(format!("unknown upload {id}"))
    }

    fn record(&self, id: &UploadId) -> Result<Arc<tokio::sync::Mutex<UploadRecord>>, RuntimeError> {
        self.records
            .lock()
            .expect("upload map mutex poisoned")
            .get(id)
            .cloned()
            .ok_or_else(|| Self::unknown(id))
    }

    fn forget(&self, id: &UploadId) {
        self.records
            .lock()
            .expect("upload map mutex poisoned")
            .remove(id);
    }

    async fn discard(&self, id: &UploadId, record: &mut UploadRecord) -> Result<(), RuntimeError> {
        // Keep a terminal record when cleanup fails or is cancelled. In
        // particular, a live S3 multipart handle cannot be found by listing.
        record.phase = UploadPhase::Aborted;
        record.store.abort_upload(id).await?;
        record.phase = UploadPhase::Finished;
        self.forget(id);
        Ok(())
    }

    async fn discard_after_error(&self, id: &UploadId, record: &mut UploadRecord) {
        if let Err(error) = self.discard(id, record).await {
            tracing::warn!(upload_id = %id, actor = %record.actor, %error, "upload cleanup failed; sweep will retry");
        }
    }

    async fn require_live(
        &self,
        id: &UploadId,
        record: &mut UploadRecord,
    ) -> Result<(), RuntimeError> {
        if record.phase != UploadPhase::Active {
            if record.phase != UploadPhase::Finished {
                self.discard_after_error(id, record).await;
            }
            return Err(Self::unknown(id));
        }
        if record.last_part.elapsed() >= self.policy.idle_for {
            self.discard_after_error(id, record).await;
            return Err(Self::unknown(id));
        }
        Ok(())
    }

    pub(crate) async fn begin(
        &self,
        size: u64,
        expected_ref: Option<ContentRef>,
        actor: String,
    ) -> Result<Value, RuntimeError> {
        let store = self.write_store()?;
        if size > MAX_OBJECT_BYTES {
            return Err(RuntimeError::InvalidInput(format!(
                "blob.begin: size exceeds the {MAX_OBJECT_BYTES}-byte maximum"
            )));
        }
        if let Some(reference) = &expected_ref {
            if let Some(stored_size) = store.size(reference).await? {
                return Ok(json!({"content_ref": reference.to_string(), "size": stored_size}));
            }
        }
        let last_part = Instant::now();
        let id = store.begin_upload(size).await?;
        let record = UploadRecord {
            store,
            size,
            expected_ref,
            hasher: blake3::Hasher::new(),
            received_bytes: 0,
            next_index: 0,
            tail: None,
            actor,
            last_part,
            phase: UploadPhase::Active,
        };
        self.records
            .lock()
            .expect("upload map mutex poisoned")
            .insert(id.clone(), Arc::new(tokio::sync::Mutex::new(record)));
        Ok(
            json!({"upload_id": id.to_string(), "part_limit": max_request_part_raw_bytes(), "next_index": 0}),
        )
    }

    pub(crate) async fn put_part(
        &self,
        id: &UploadId,
        index: u64,
        bytes: Vec<u8>,
    ) -> Result<Value, RuntimeError> {
        self.write_store()?;
        let entry = self.record(id)?;
        let mut record = entry.lock().await;
        self.require_live(id, &mut record).await?;
        let part_hash = blake3::hash(&bytes);
        if record.next_index.checked_sub(1) == Some(index) {
            let (tail_len, tail_hash) = record.tail.expect("accepted part has a tail digest");
            if bytes.len() != tail_len || part_hash != tail_hash {
                self.discard_after_error(id, &mut record).await;
                return Err(RuntimeError::InvalidInput(
                    "blob.put_part: tail retry differs from the accepted part; upload aborted"
                        .into(),
                ));
            }
            // A retry did not write a new part or refresh the backend mtime;
            // keep its idle clock unchanged as well.
            return Ok(
                json!({"next_index": record.next_index, "received_bytes": record.received_bytes}),
            );
        }
        if index != record.next_index {
            return Err(RuntimeError::InvalidInput(format!(
                "blob.put_part: expected index {}, got {index}",
                record.next_index
            )));
        }
        let received = record.received_bytes.checked_add(bytes.len() as u64);
        let Some(received) =
            received.filter(|total| *total <= record.size && *total <= MAX_OBJECT_BYTES)
        else {
            self.discard_after_error(id, &mut record).await;
            return Err(RuntimeError::InvalidInput(
                "blob.put_part: part exceeds declared size; upload aborted".into(),
            ));
        };
        if bytes.len() as u64 > max_request_part_raw_bytes() {
            return Err(RuntimeError::InvalidInput(format!(
                "blob.put_part: decoded length {} exceeds part_limit {}",
                bytes.len(),
                max_request_part_raw_bytes()
            )));
        }
        let Some(next_index) = record.next_index.checked_add(1) else {
            self.discard_after_error(id, &mut record).await;
            return Err(RuntimeError::InvalidInput(
                "blob.put_part: index overflow; upload aborted".into(),
            ));
        };
        let mut hasher = record.hasher.clone();
        hasher.update(&bytes);
        let part_len = bytes.len();
        // If this await is cancelled, the backend may still finish its write.
        // Leave InFlight until success is accounted for: later verbs abort it.
        let accepted_at = Instant::now();
        record.phase = UploadPhase::InFlight;
        match record.store.append_part(id, bytes).await {
            Ok(observed) if observed == received => {}
            Ok(_) => {
                self.discard_after_error(id, &mut record).await;
                return Err(RuntimeError::Internal(
                    "staged upload length differs from accepted parts".into(),
                ));
            }
            Err(error) => {
                self.discard_after_error(id, &mut record).await;
                return Err(error.into());
            }
        }
        record.hasher = hasher;
        record.received_bytes = received;
        record.next_index = next_index;
        record.tail = Some((part_len, part_hash));
        // Start before backend I/O so a slow sync cannot leave the staging
        // mtime expired while the pack clock claims the part is still fresh.
        record.last_part = accepted_at;
        record.phase = UploadPhase::Active;
        Ok(json!({"next_index": next_index, "received_bytes": received}))
    }

    pub(crate) async fn reject_oversized_part(
        &self,
        id: &UploadId,
        index: u64,
        decoded_bytes: u64,
    ) -> Result<Value, RuntimeError> {
        self.write_store()?;
        let entry = self.record(id)?;
        let mut record = entry.lock().await;
        self.require_live(id, &mut record).await?;
        // Every accepted tail fits the part limit. A validated encoded
        // input whose decoded size exceeds it cannot be an identical retry.
        let changed_tail = record.next_index.checked_sub(1) == Some(index);
        let crosses_size = index == record.next_index
            && record.received_bytes.saturating_add(decoded_bytes) > record.size;
        if changed_tail || crosses_size {
            self.discard_after_error(id, &mut record).await;
        }
        Err(RuntimeError::InvalidInput(format!(
            "blob.put_part: base64 input exceeds the {}-byte part_limit",
            max_request_part_raw_bytes()
        )))
    }

    pub(crate) async fn commit(&self, id: &UploadId) -> Result<Value, RuntimeError> {
        self.write_store()?;
        let entry = self.record(id)?;
        let mut record = entry.lock().await;
        self.require_live(id, &mut record).await?;
        if record.received_bytes != record.size {
            return Err(RuntimeError::InvalidInput(format!(
                "blob.commit: received {} bytes, expected {}",
                record.received_bytes, record.size
            )));
        }
        let reference = ContentRef::from_digest_bytes(record.hasher.finalize().as_bytes());
        if record
            .expected_ref
            .as_ref()
            .is_some_and(|expected| expected != &reference)
        {
            self.discard_after_error(id, &mut record).await;
            return Err(RuntimeError::InvalidInput(
                "blob.commit: content_ref mismatch; upload aborted".into(),
            ));
        }
        record.phase = UploadPhase::InFlight;
        if let Err(error) = record.store.commit_upload(id, &reference).await {
            self.discard_after_error(id, &mut record).await;
            return Err(error.into());
        }
        record.phase = UploadPhase::Finished;
        self.forget(id);
        Ok(json!({"content_ref": reference.to_string(), "size": record.size}))
    }

    pub(crate) async fn abort(&self, id: &UploadId) -> Result<Value, RuntimeError> {
        self.write_store()?;
        let entry = self.record(id)?;
        let mut record = entry.lock().await;
        if record.phase == UploadPhase::Finished {
            return Err(Self::unknown(id));
        }
        self.discard(id, &mut record).await?;
        Ok(json!({"aborted": true}))
    }

    /// Expire records and sweep backend staging that has lost its process state.
    pub async fn sweep(&self) -> Result<u64, RuntimeError> {
        let store = self.write_store()?;
        let entries: Vec<_> = self
            .records
            .lock()
            .expect("upload map mutex poisoned")
            .iter()
            .map(|(id, record)| (id.clone(), Arc::clone(record)))
            .collect();
        let mut removed = 0;
        let mut failure = None;
        for (id, entry) in entries {
            let mut record = entry.lock().await;
            if record.phase == UploadPhase::Finished {
                continue;
            }
            if record.phase != UploadPhase::Active
                || record.last_part.elapsed() >= self.policy.idle_for
            {
                match self.discard(&id, &mut record).await {
                    Ok(()) => removed += 1,
                    Err(error) => {
                        tracing::warn!(upload_id = %id, actor = %record.actor, %error, "upload expiry failed; next tick will retry");
                        failure.get_or_insert(error);
                    }
                }
            }
        }
        match store.sweep_uploads(self.policy.idle_for).await {
            Ok(count) => removed += count,
            Err(error) => {
                failure.get_or_insert(error.into());
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(removed),
        }
    }
}

#[cfg(test)]
mod tests;
