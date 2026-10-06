//! Explicit receipt custody for tests and benchmarks that perform memory writes.
//! This source is included only by test and benchmark targets.

use std::sync::Arc;

use khive_runtime::credentials::{
    CredentialCacheLifetime, CredentialConfig, CredentialError, CredentialKind, CredentialMaterial,
    CredentialProvider, CredentialRegistry, VisibilityReceiptConfig, VisibilityReceiptKeyConfig,
};
use khive_runtime::KhiveRuntime;

struct TestReceiptProvider;

impl CredentialProvider for TestReceiptProvider {
    fn resolve(&self, name: &str) -> Result<CredentialMaterial, CredentialError> {
        if name != "test-receipt" {
            return Err(CredentialError::UnknownCredential {
                name: name.to_owned(),
            });
        }
        // Canonical base64url for 32 zero bytes, deliberately public test material.
        Ok(CredentialMaterial::new(vec![b'A'; 43]))
    }

    fn cache_lifetime(&self) -> CredentialCacheLifetime {
        CredentialCacheLifetime::NoCache
    }
}

pub fn with_receipt_credentials(runtime: KhiveRuntime) -> KhiveRuntime {
    let mut credentials = CredentialRegistry::new(vec![CredentialConfig {
        name: "test-receipt".into(),
        kind: CredentialKind::SigningKey,
        provider: "test-receipt-provider".into(),
        env_var: None,
        header: None,
    }])
    .expect("valid test receipt declaration");
    credentials
        .register_provider(
            "test-receipt-provider".into(),
            Arc::new(TestReceiptProvider),
        )
        .expect("register test receipt provider");
    runtime
        .with_visibility_receipt_credentials(
            VisibilityReceiptConfig {
                keys: vec![VisibilityReceiptKeyConfig {
                    id: "test-receipt-key".into(),
                    credential: "test-receipt".into(),
                    encrypt: true,
                }],
            },
            Arc::new(credentials),
        )
        .expect("install explicit test receipt custody")
}
