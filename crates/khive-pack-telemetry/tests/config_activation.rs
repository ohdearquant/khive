use std::collections::HashMap;

use khive_pack_kg::KgPack;
use khive_pack_telemetry::TelemetryPack;
use khive_runtime::{
    KhiveRuntime, PackRegistry, RuntimeConfig, RuntimeError, TelemetryCarrier, TelemetryConfig,
    VerbRegistryBuilder,
};

fn runtime(default_carrier: Option<TelemetryCarrier>) -> KhiveRuntime {
    KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: vec!["kg".into()],
        brain_profile: None,
        telemetry: TelemetryConfig {
            default_carrier,
            ..TelemetryConfig::default()
        },
        ..RuntimeConfig::no_embeddings()
    })
    .expect("in-memory runtime")
}

fn builder(runtime: &KhiveRuntime, telemetry: bool) -> VerbRegistryBuilder {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    if telemetry {
        builder.register(TelemetryPack::new(runtime.clone()));
    }
    builder
}

fn assert_missing_default(error: RuntimeError) {
    assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error}");
    assert!(
        error.to_string().contains("telemetry.default_carrier"),
        "{error}"
    );
}

#[test]
fn manual_registration_requires_default_only_for_loaded_telemetry() {
    let missing = runtime(None);
    let kg_only = builder(&missing, false)
        .build()
        .expect("telemetry is absent");
    assert!(kg_only.has_verb("stream.read"));
    assert!(!kg_only.has_verb("telemetry.emit"));
    assert_missing_default(
        builder(&missing, true)
            .build()
            .err()
            .expect("missing default"),
    );

    for carrier in [TelemetryCarrier::Durable, TelemetryCarrier::Ephemeral] {
        let declared = runtime(Some(carrier));
        let loaded = builder(&declared, true).build().expect("declared default");
        assert!(loaded.has_verb("telemetry.emit"));
    }
}

#[test]
fn metadata_inspection_is_available_without_activating_telemetry() {
    let missing = runtime(None);
    let metadata = builder(&missing, true)
        .build_metadata()
        .expect("metadata only");
    assert!(metadata.has_verb("telemetry.emit"));
    assert!(metadata.describe_verb("telemetry.channels").is_ok());
    assert_eq!(metadata.pack_requires("telemetry"), Some(&["kg"][..]));
    assert_missing_default(
        builder(&missing, true)
            .build()
            .err()
            .expect("activation refuses"),
    );

    let mut incomplete = VerbRegistryBuilder::new();
    incomplete.register(TelemetryPack::new(missing));
    assert!(matches!(
        incomplete.build_metadata(),
        Err(RuntimeError::MissingPackDependency(_))
    ));
}

#[test]
fn per_pack_runtime_controls_telemetry_activation() {
    let missing = runtime(None);
    let declared = runtime(Some(TelemetryCarrier::Durable));
    let names = vec!["kg".into(), "telemetry".into()];
    for (base, telemetry, succeeds) in [(&missing, &declared, true), (&declared, &missing, false)] {
        let runtimes = HashMap::from([("telemetry".into(), telemetry.clone())]);
        let mut builder = VerbRegistryBuilder::new();
        PackRegistry::register_packs_with_runtimes(&names, &runtimes, base, &mut builder)
            .expect("factories registered");
        match builder.build() {
            Ok(registry) => {
                assert!(
                    succeeds,
                    "the default runtime must not mask the pack's missing key"
                );
                assert!(registry.has_verb("telemetry.emit"));
            }
            Err(error) => {
                assert!(
                    !succeeds,
                    "the pack's own declaration should permit activation"
                );
                assert_missing_default(error);
            }
        }
    }
}
