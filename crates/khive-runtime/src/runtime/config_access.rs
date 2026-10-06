use super::*;

impl KhiveRuntime {
    /// Return the extra-visible namespaces assembled at config load.
    ///
    /// OSS dispatch uses this set to widen the default multi-record read scope
    /// to `['local'] ∪ visible_namespaces`. Writes are unchanged: always
    /// pinned to `'local'`. This set is also available as gate/cloud policy
    /// input.
    pub fn visible_namespaces(&self) -> &[Namespace] {
        &self.config.visible_namespaces
    }

    pub(crate) fn install_visibility_receipt_capability(
        &mut self,
        ring: crate::credentials::VisibilityReceiptConfig,
        credentials: Vec<crate::credentials::CredentialConfig>,
        capability: crate::visibility_receipts::ReceiptCapability,
    ) {
        self.config.credentials = credentials;
        self.config.visibility_receipts = Some(ring);
        self.visibility_receipts = Arc::new(capability);
    }

    /// Return a reference to the runtime config.
    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    /// Install the host's full declared SQLite topology before pack registration.
    pub fn with_declared_backend_db_paths(mut self, paths: Arc<[PathBuf]>) -> Self {
        self.declared_backend_db_paths = paths;
        self
    }

    /// All declared SQLite backend paths known to this runtime's host.
    pub fn declared_backend_db_paths(&self) -> &[PathBuf] {
        &self.declared_backend_db_paths
    }
}
