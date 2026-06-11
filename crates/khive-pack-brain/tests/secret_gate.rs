//! Secret-gate regression tests for brain pack write paths.
//!
//! Verifies that `brain.create_profile` and `brain.bind` reject
//! credential-shaped content before any write is persisted.

use khive_pack_brain::BrainPack;
use khive_runtime::{
    KhiveRuntime, Namespace, NamespaceToken, PackRuntime, RuntimeError, VerbRegistryBuilder,
};
use serde_json::json;

fn is_secret_detected(err: &RuntimeError) -> bool {
    matches!(err, RuntimeError::SecretDetected(_))
}

/// Shared context so multiple calls can operate on the same runtime.
struct BrainCtx {
    brain: BrainPack,
    registry: khive_runtime::VerbRegistry,
    token: NamespaceToken,
}

impl BrainCtx {
    fn new() -> Self {
        let rt = KhiveRuntime::memory().expect("in-memory runtime");
        let brain = BrainPack::new(rt.clone());
        let registry = VerbRegistryBuilder::new()
            .build()
            .expect("minimal registry");
        let token = rt
            .authorize(Namespace::local())
            .expect("authorize local token");
        Self {
            brain,
            registry,
            token,
        }
    }

    async fn dispatch(
        &self,
        verb: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RuntimeError> {
        self.brain
            .dispatch(verb, params, &self.registry, &self.token)
            .await
    }
}

/// brain.create_profile with a fake AWS key in description must be blocked.
#[tokio::test]
async fn create_profile_blocks_secret_in_description() {
    let ctx = BrainCtx::new();
    let result = ctx
        .dispatch(
            "brain.create_profile",
            json!({
                "name": "test-profile",
                "description": "AWS key: AKIAFAKEKEY000000000", // gitleaks:allow
            }),
        )
        .await;
    assert!(
        result.as_ref().err().is_some_and(is_secret_detected),
        "create_profile with secret in description must be rejected; got: {result:?}"
    );
}

/// brain.create_profile with a clean description must succeed.
#[tokio::test]
async fn create_profile_clean_description_passes() {
    let ctx = BrainCtx::new();
    let result = ctx
        .dispatch(
            "brain.create_profile",
            json!({
                "name": "clean-profile",
                "description": "A normal profile for recall operations.",
            }),
        )
        .await;
    assert!(
        result.is_ok(),
        "create_profile with clean description must succeed; got: {result:?}"
    );
}

/// brain.bind with a fake credential as actor must be blocked.
///
/// The gate must fire BEFORE any profile-existence check, so the order of
/// validation (secret check first) is part of the contract under test.
#[tokio::test]
async fn bind_blocks_secret_in_actor() {
    let ctx = BrainCtx::new();

    // Create a valid profile in the same runtime so bind can find it.
    ctx.dispatch(
        "brain.create_profile",
        json!({ "name": "profile-for-bind" }),
    )
    .await
    .expect("clean profile creation must succeed");

    // Now try to bind with a fake AWS key as the actor name.
    let result = ctx
        .dispatch(
            "brain.bind",
            json!({
                "profile_id": "profile-for-bind",
                "actor": "AKIAFAKEKEY000000000", // gitleaks:allow
            }),
        )
        .await;
    assert!(
        result.as_ref().err().is_some_and(is_secret_detected),
        "brain.bind with secret in actor must be rejected; got: {result:?}"
    );
}
