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

    /// Snapshot import vocabulary without treating a poisoned registry as an
    /// unknown kind. Ordinary create validation retains its existing semantics.
    pub(crate) fn import_entity_kind_registry(&self) -> RuntimeResult<Vec<String>> {
        self.valid_entity_kinds
            .read()
            .map(|kinds| kinds.clone())
            .map_err(|_| RuntimeError::Internal("entity kind registry lock poisoned".into()))
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

#[cfg(test)]
mod import_registry_tests {
    use super::*;
    #[tokio::test]
    async fn relaxed_import_propagates_poisoned_kind_registry() {
        let runtime = KhiveRuntime::memory().unwrap();
        let token = runtime.authorize(Namespace::local()).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = runtime.valid_entity_kinds.write().unwrap();
            panic!("poison import vocabulary");
        }));
        assert!(result.is_err());
        let input = r#"{"format":"khive-kg","version":"0.1","namespace":"local","exported_at":"2026-01-01T00:00:00Z","entities":[{"id":"00000000-0000-0000-0000-000000000001","kind":"Future","name":"Unwritten","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}],"edges":[]}"#;
        let error = runtime
            .import_kg_json_with_policy(
                input,
                &token,
                khive_types::ImportKindPolicy::PreserveUnknown,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, RuntimeError::Internal(_)), "{error}");
        assert!(error.to_string().contains("poisoned"));
        assert!(runtime
            .get_entity(&token, uuid::Uuid::from_u128(1))
            .await
            .is_err());
    }
}
