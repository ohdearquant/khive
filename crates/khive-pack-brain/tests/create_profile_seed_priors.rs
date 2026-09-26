//! Creation-time seed validation for brain profiles.

use khive_pack_brain::BrainPack;
use khive_runtime::{KhiveRuntime, Namespace, PackRuntime, RuntimeError, VerbRegistryBuilder};
use serde_json::json;

#[tokio::test]
async fn create_profile_rejects_unknown_seed_field_without_creating_profile() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let brain = BrainPack::new(runtime.clone());
    let registry = VerbRegistryBuilder::new()
        .build()
        .expect("minimal registry");
    let token = runtime
        .authorize(Namespace::local())
        .expect("authorize local token");

    let error = brain
        .dispatch(
            "brain.create_profile",
            json!({
                "name": "seeded-profile",
                "seed_priors": {
                    "section_posteriors": {"overview": {"alpha": 2.0, "beta": 2.0}},
                    "relevance": {"alpha": 7.0, "beta": 3.0}
                }
            }),
            &registry,
            &token,
        )
        .await
        .expect_err("unsupported recall seed must not be silently ignored");
    assert!(
        matches!(error, RuntimeError::InvalidInput(message) if message.contains("relevance")),
        "the unsupported field should be named"
    );

    let created = brain
        .dispatch(
            "brain.create_profile",
            json!({
                "name": "seeded-profile",
                "seed_priors": {
                    "section_posteriors": {"overview": {"alpha": 2.0, "beta": 2.0}}
                }
            }),
            &registry,
            &token,
        )
        .await
        .expect("valid section seed should still create the profile");
    assert_eq!(created["created"], true);
}
