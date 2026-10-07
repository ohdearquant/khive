//! Runtime custody and authorization for opaque memory visibility receipts.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use khive_types::{Details, ErrorKind, KhiveError};
use uuid::Uuid;

use crate::credentials::receipt_sealer::{ReceiptFields, ReceiptSealError, ReceiptSealer};
use crate::credentials::{CredentialRegistry, VisibilityReceiptConfig};
use crate::{KhiveRuntime, RuntimeConfig, RuntimeError, RuntimeResult};

const MAX_AGE_MS: i64 = 24 * 60 * 60 * 1_000;
const MAX_FUTURE_MS: i64 = 5 * 60 * 1_000;
const REPLAY_PHASE: &str = "exact_replay";

/// Positive receipt admission shared by clones bound to one runtime backend.
/// Construction performs no check; the first successful check latches, a failed
/// one stays retryable, and unlatched calls retain synchronous validation.
pub(crate) struct ReceiptCutover {
    backend: Arc<khive_db::StorageBackend>,
    ready: AtomicBool,
    check: parking_lot::Mutex<()>,
}

impl ReceiptCutover {
    pub(crate) fn new(backend: Arc<khive_db::StorageBackend>, validated: bool) -> Self {
        Self {
            backend,
            ready: AtomicBool::new(validated),
            check: parking_lot::Mutex::new(()),
        }
    }

    pub(crate) fn is_bound_to(&self, backend: &Arc<khive_db::StorageBackend>) -> bool {
        Arc::ptr_eq(&self.backend, backend)
    }

    fn ensure(&self) -> RuntimeResult<()> {
        if self.ready.load(Ordering::Acquire) {
            return Ok(());
        }
        let _check = self.check.lock();
        if self.ready.load(Ordering::Acquire) {
            return Ok(());
        }
        self.backend
            .validate_memory_visibility_cutover()
            .map_err(|_| receipt_failure("receipt_store_unavailable", None, false))?;
        self.ready.store(true, Ordering::Release);
        Ok(())
    }

    /// Validate again after a receipt-bearing write rolled back, so a receipt
    /// store that became unusable after the latch was set is still refused as
    /// `receipt_store_unavailable`. A failed check clears the latch.
    fn recheck(&self) -> RuntimeResult<()> {
        let _check = self.check.lock();
        if self.backend.validate_memory_visibility_cutover().is_ok() {
            return Ok(());
        }
        self.ready.store(false, Ordering::Release);
        Err(receipt_failure("receipt_store_unavailable", None, false))
    }
}

pub(crate) enum ReceiptCapability {
    Absent,
    Configured(ReceiptSealer),
    Unavailable,
}

impl ReceiptCapability {
    pub(crate) fn from_config(config: &RuntimeConfig) -> Self {
        let Some(ring) = &config.visibility_receipts else {
            return Self::Absent;
        };
        let Ok(registry) = CredentialRegistry::new(config.credentials.clone()) else {
            return Self::Unavailable;
        };
        match ReceiptSealer::new(ring.clone(), Arc::new(registry)) {
            Ok(sealer) => Self::Configured(sealer),
            Err(_) => Self::Unavailable,
        }
    }

    fn sealer(&self) -> RuntimeResult<&ReceiptSealer> {
        match self {
            Self::Configured(sealer) => Ok(sealer),
            Self::Absent | Self::Unavailable => {
                Err(receipt_failure("visibility_key_unavailable", None, false))
            }
        }
    }
}

/// Authenticated and scope-checked receipt fields for trusted proof consumers.
/// Deliberately has no formatting or serialization implementation.
pub struct AuthenticatedVisibilityReceipt(ReceiptFields);

impl AuthenticatedVisibilityReceipt {
    pub fn namespace(&self) -> &str {
        &self.0.namespace
    }

    pub fn sequence_for_model(&self, model: &str) -> Option<u64> {
        self.0
            .fences
            .iter()
            .find(|(name, _)| name == model)
            .map(|(_, seq)| *seq)
    }
}

fn invalid_receipt() -> RuntimeError {
    RuntimeError::InvalidInput("memory.recall invalid visibility receipt".into())
}

fn seal_error(error: ReceiptSealError) -> RuntimeError {
    match error {
        ReceiptSealError::KeyUnavailable => {
            receipt_failure("visibility_key_unavailable", None, false)
        }
        ReceiptSealError::NonceUnavailable => {
            receipt_failure("visibility_nonce_unavailable", None, false)
        }
        ReceiptSealError::InvalidReceipt => invalid_receipt(),
    }
}

pub(crate) fn receipt_failure(
    reason: &'static str,
    memory_id: Option<Uuid>,
    replay: bool,
) -> RuntimeError {
    let mut fields = vec![("reason", reason.to_owned())];
    if let Some(id) = memory_id {
        fields.push(("memory_id", id.to_string()));
    }
    if replay {
        fields.push(("receipt_phase", REPLAY_PHASE.to_owned()));
    }
    KhiveError::unavailable("freshness_unmet: memory visibility receipt unavailable")
        .with_details(Details::new_owned(fields))
        .into()
}

/// A closed projection; arbitrary detail values cannot assert write disposition.
pub(crate) fn receipt_error_projection(error: &RuntimeError) -> Option<(bool, bool)> {
    let RuntimeError::Khive(error) = error else {
        return None;
    };
    if error.kind() != ErrorKind::Unavailable {
        return None;
    }
    let details = error.details()?;
    let reason = details.get("reason")?;
    let retryable = match reason {
        "receipt_temporarily_unavailable"
        | "receipt_store_unavailable"
        | "visibility_key_unavailable"
        | "visibility_nonce_unavailable"
        | "visibility_receipt_unavailable" => true,
        "legacy_receipt_absent" | "receipt_epoch_unknown" | "visibility_token_expired" => false,
        _ => return None,
    };
    let exact_replay = matches!(
        reason,
        "receipt_temporarily_unavailable" | "legacy_receipt_absent" | "receipt_epoch_unknown"
    ) && details.get("receipt_phase") == Some(REPLAY_PHASE)
        && details
            .get("memory_id")
            .is_some_and(|id| Uuid::parse_str(id).is_ok());
    Some((retryable, exact_replay))
}

fn validate_fields(
    fields: ReceiptFields,
    namespaces: &[&str],
    models: &[String],
    now: i64,
) -> RuntimeResult<AuthenticatedVisibilityReceipt> {
    if !namespaces.contains(&fields.namespace.as_str())
        || fields.fences.iter().any(|(name, _)| !models.contains(name))
    {
        return Err(invalid_receipt());
    }
    if fields.issued_at > now.saturating_add(MAX_FUTURE_MS) {
        return Err(invalid_receipt());
    }
    if fields.issued_at < now.saturating_sub(MAX_AGE_MS) {
        return Err(receipt_failure("visibility_token_expired", None, false));
    }
    Ok(AuthenticatedVisibilityReceipt(fields))
}

impl KhiveRuntime {
    /// Return a fixed startup notice for absent or unusable receipt custody.
    ///
    /// This only projects configuration state: it neither resolves credentials
    /// nor reads storage. `None` means custody was configured, not that its
    /// provider is currently available. Memory-pack activation logs this notice;
    /// hosts serving memory through direct runtime APIs may report it themselves.
    pub fn visibility_receipt_custody_notice(&self) -> Option<&'static str> {
        match self.visibility_receipts.as_ref() {
            ReceiptCapability::Absent => Some(
                "no [visibility_receipts] section is configured: memory.remember stores memories \
                 without a visibility token and session recall refuses until receipt keys are \
                 configured",
            ),
            ReceiptCapability::Unavailable => Some(
                "configured [visibility_receipts] custody is unusable: memory.remember and \
                 session recall refuse with visibility_key_unavailable; check configuration",
            ),
            ReceiptCapability::Configured(_) => None,
        }
    }

    /// Install host-owned custody before cloning or passing the runtime to packs.
    /// The registry exposes references/providers; resolved material stays private.
    pub fn with_visibility_receipt_credentials(
        mut self,
        ring: VisibilityReceiptConfig,
        credentials: Arc<CredentialRegistry>,
    ) -> RuntimeResult<Self> {
        let declarations = credentials.declarations();
        let sealer = ReceiptSealer::new(ring.clone(), credentials).map_err(seal_error)?;
        self.install_visibility_receipt_capability(
            ring,
            declarations,
            ReceiptCapability::Configured(sealer),
        );
        Ok(self)
    }

    pub(crate) fn require_visibility_cutover(&self) -> RuntimeResult<()> {
        self.visibility_cutover.ensure()
    }

    /// Re-check the cutover after a receipt-bearing write rolled back.
    pub(crate) fn recheck_visibility_cutover(&self) -> RuntimeResult<()> {
        self.visibility_cutover.recheck()
    }

    /// Verify current key availability without issuing a receipt or reserving a nonce.
    pub fn ensure_visibility_receipt_key(&self) -> RuntimeResult<()> {
        self.require_visibility_cutover()?;
        self.visibility_receipts
            .sealer()?
            .ensure_key()
            .map_err(seal_error)
    }

    /// Preflight for a writer that proceeds without custody. `Ok(true)` means
    /// custody is configured and its key resolves, so the write must be sealed.
    /// `Ok(false)` means no receipt configuration exists. A schema that has not
    /// completed the cutover, a configuration that cannot be used and a key
    /// that cannot be resolved all refuse.
    pub fn ensure_visibility_receipt_key_if_configured(&self) -> RuntimeResult<bool> {
        self.require_visibility_cutover()?;
        if matches!(*self.visibility_receipts, ReceiptCapability::Absent) {
            return Ok(false);
        }
        self.visibility_receipts
            .sealer()?
            .ensure_key()
            .map_err(seal_error)?;
        Ok(true)
    }

    /// Seal exact original fences. Never reconstructs fences from the current log.
    pub fn seal_visibility_receipt(
        &self,
        namespace: &str,
        fences: &[(String, u64)],
    ) -> RuntimeResult<String> {
        self.require_visibility_cutover()?;
        self.visibility_receipts
            .sealer()?
            .seal(namespace, fences)
            .map_err(seal_error)
    }

    /// Authenticate and constrain a token to the already-authorized effective scope.
    pub fn open_visibility_receipt(
        &self,
        token: &str,
        effective_namespaces: &[&str],
        requested_models: &[String],
    ) -> RuntimeResult<AuthenticatedVisibilityReceipt> {
        self.open_visibility_receipt_at(
            token,
            effective_namespaces,
            requested_models,
            chrono::Utc::now().timestamp_millis(),
        )
    }

    fn open_visibility_receipt_at(
        &self,
        token: &str,
        effective_namespaces: &[&str],
        requested_models: &[String],
        now: i64,
    ) -> RuntimeResult<AuthenticatedVisibilityReceipt> {
        self.require_visibility_cutover()?;
        ReceiptSealer::validate_envelope(token).map_err(seal_error)?;
        let fields = self
            .visibility_receipts
            .sealer()?
            .open(token)
            .map_err(seal_error)?;
        validate_fields(fields, effective_namespaces, requested_models, now)
    }
}

#[cfg(test)]
#[path = "visibility_receipts_tests.rs"]
mod tests;
