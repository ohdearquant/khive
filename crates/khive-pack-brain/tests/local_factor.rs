//! `local_factor` on profile creation: validation, storage, and read-back.

use khive_pack_brain::BrainPack;
use khive_runtime::{
    KhiveRuntime, Namespace, NamespaceToken, PackRuntime, RuntimeError, VerbRegistry,
    VerbRegistryBuilder,
};
use serde_json::{json, Value};

struct Fixture {
    brain: BrainPack,
    registry: VerbRegistry,
    token: NamespaceToken,
}

impl Fixture {
    async fn new() -> Self {
        let runtime = KhiveRuntime::memory().expect("in-memory runtime");
        let brain = BrainPack::new(runtime.clone());
        let registry = VerbRegistryBuilder::new()
            .build()
            .expect("minimal registry");
        let token = runtime
            .authorize(Namespace::local())
            .expect("authorize local token");
        let fixture = Self {
            brain,
            registry,
            token,
        };
        fixture
            .dispatch("brain.profiles", json!({}))
            .await
            .expect("initialise the profile registry");
        fixture
    }

    async fn dispatch(&self, verb: &str, params: Value) -> Result<Value, RuntimeError> {
        self.brain
            .dispatch(verb, params, &self.registry, &self.token)
            .await
    }

    async fn profile(&self, id: &str) -> Value {
        self.dispatch("brain.profile", json!({ "profile_id": id }))
            .await
            .expect("brain.profile")
    }

    async fn create(&self, name: &str, extra: Value) -> Value {
        let mut params = json!({ "name": name, "consumer_kind": "recall" });
        for (key, value) in extra.as_object().expect("extra params object") {
            params[key] = value.clone();
        }
        self.dispatch("brain.create_profile", params)
            .await
            .expect("brain.create_profile")
    }
}

/// A value that is not a finite number in `[0.5, 1]` is refused with an invalid-input error that
/// names the range, and no profile is created.
///
/// NaN and infinity have no JSON number form, so they cannot arrive through this verb as
/// numbers; the unit tests beside the validator check that the values `serde_json` builds for
/// them are refused as well.
#[tokio::test]
async fn create_profile_refuses_local_factor_outside_the_contract_and_creates_nothing() {
    let f = Fixture::new().await;
    let before = f
        .dispatch("brain.profiles", json!({}))
        .await
        .expect("profiles before");

    let refused = [
        json!("0.7"),
        json!(true),
        json!(null),
        json!([0.7]),
        json!(0.49),
        json!(1.01),
        json!(0.0),
        json!(-0.7),
        json!(1e300),
    ];
    for value in refused {
        let error = f
            .dispatch(
                "brain.create_profile",
                json!({ "name": "refused-factor", "local_factor": value.clone() }),
            )
            .await
            .expect_err("an out-of-contract local_factor must be refused");
        let RuntimeError::InvalidInput(message) = error else {
            panic!("local_factor {value}: expected InvalidInput, got {error:?}");
        };
        assert!(
            message.contains("[0.5, 1]"),
            "local_factor {value}: the refusal must name the range, got {message:?}"
        );
    }

    assert!(
        f.dispatch("brain.profile", json!({ "profile_id": "refused-factor" }))
            .await
            .is_err(),
        "a refused create_profile must not leave a profile behind"
    );
    let after = f
        .dispatch("brain.profiles", json!({}))
        .await
        .expect("profiles after");
    assert_eq!(
        after["count"], before["count"],
        "a refused create_profile must not change the profile count"
    );
}

/// Both ends of the range are accepted and `brain.profile` returns the stored number; a profile
/// created without the field reports `null`, which means a factor of 1.
#[tokio::test]
async fn profile_reports_the_stored_local_factor_or_null_when_absent() {
    let f = Fixture::new().await;

    f.create("factor-low", json!({ "local_factor": 0.5 })).await;
    f.create("factor-mid", json!({ "local_factor": 0.7 })).await;
    f.create("factor-one", json!({ "local_factor": 1 })).await;
    f.create("factor-none", json!({})).await;

    assert_eq!(
        f.profile("factor-low").await["local_factor"].as_f64(),
        Some(0.5)
    );
    assert_eq!(
        f.profile("factor-mid").await["local_factor"].as_f64(),
        Some(0.7)
    );
    assert_eq!(
        f.profile("factor-one").await["local_factor"].as_f64(),
        Some(1.0)
    );
    assert!(
        f.profile("factor-none").await["local_factor"].is_null(),
        "a profile created without local_factor reports null"
    );
}

/// No verb edits the factor: `brain.reset` keeps it.
#[tokio::test]
async fn reset_keeps_the_local_factor() {
    let f = Fixture::new().await;
    f.create("factor-reset", json!({ "local_factor": 0.6 }))
        .await;

    f.dispatch("brain.reset", json!({ "profile_id": "factor-reset" }))
        .await
        .expect("brain.reset");

    assert_eq!(
        f.profile("factor-reset").await["local_factor"].as_f64(),
        Some(0.6),
        "brain.reset must not clear the factor"
    );
}

/// `brain.profile` returns the mean of each stored posterior as top-level `posterior_means`: a
/// new recall profile starts at the priors Beta(7, 3), Beta(2, 8) and Beta(1, 9), whose means are
/// 0.7, 0.2 and 0.1, and the values returned match the stored snapshot in the same response.
#[tokio::test]
async fn profile_reports_posterior_means_of_the_stored_posteriors() {
    let f = Fixture::new().await;
    f.create("means-profile", json!({})).await;

    let profile = f.profile("means-profile").await;
    let means = profile["posterior_means"]
        .as_object()
        .expect("posterior_means is an object for a profile with a state snapshot");
    for (key, expected) in [("relevance", 0.7), ("salience", 0.2), ("temporal", 0.1)] {
        let mean = means
            .get(key)
            .and_then(Value::as_f64)
            .unwrap_or_else(|| panic!("posterior_means.{key} must be a number: {means:?}"));
        assert!(
            (mean - expected).abs() < 1e-9,
            "posterior_means.{key} is the mean of the stored posterior: {mean} vs {expected}"
        );

        let stored = &profile["state_snapshot"][key];
        let alpha = stored["alpha"].as_f64().expect("stored alpha");
        let beta = stored["beta"].as_f64().expect("stored beta");
        assert!(
            (mean - alpha / (alpha + beta)).abs() < 1e-12,
            "posterior_means.{key} must equal alpha / (alpha + beta) of the stored posterior"
        );
    }
}

/// The factor is part of the persisted profile record: a second pack over the same store loads it
/// from the persisted snapshot and `brain.profile` returns the stored number, while a profile
/// created without one still reports `null`.
#[tokio::test]
async fn a_pack_loading_the_persisted_snapshot_keeps_the_local_factor() {
    let runtime = KhiveRuntime::memory().expect("in-memory runtime");
    let token = runtime
        .authorize(Namespace::local())
        .expect("authorize local token");
    let registry = VerbRegistryBuilder::new()
        .build()
        .expect("minimal registry");

    let writer = BrainPack::new(runtime.clone());
    for params in [
        json!({ "name": "persisted-factor", "local_factor": 0.65 }),
        json!({ "name": "persisted-none" }),
    ] {
        writer
            .dispatch("brain.create_profile", params, &registry, &token)
            .await
            .expect("brain.create_profile");
    }

    let reader = BrainPack::new(runtime.clone());
    let with_factor = reader
        .dispatch(
            "brain.profile",
            json!({ "profile_id": "persisted-factor" }),
            &registry,
            &token,
        )
        .await
        .expect("brain.profile on the reloaded pack");
    assert_eq!(with_factor["local_factor"].as_f64(), Some(0.65));

    let without_factor = reader
        .dispatch(
            "brain.profile",
            json!({ "profile_id": "persisted-none" }),
            &registry,
            &token,
        )
        .await
        .expect("brain.profile on the reloaded pack");
    assert!(without_factor["local_factor"].is_null());
}
