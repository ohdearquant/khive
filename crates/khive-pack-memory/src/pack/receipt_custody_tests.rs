use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use khive_pack_kg::KgPack;
use khive_runtime::credentials::{
    CredentialCacheLifetime, CredentialConfig, CredentialError, CredentialKind, CredentialMaterial,
    CredentialProvider, CredentialRegistry, VisibilityReceiptConfig, VisibilityReceiptKeyConfig,
};
use khive_runtime::{KhiveRuntime, PackRegistry, RuntimeConfig, VerbRegistryBuilder};

use super::MemoryPack;

const ABSENT_NOTICE: &str = "no [visibility_receipts] section is configured: memory.remember stores memories without a visibility token and session recall refuses until receipt keys are configured";
const UNAVAILABLE_NOTICE: &str = "configured [visibility_receipts] custody is unusable: memory.remember and session recall refuse with visibility_key_unavailable; check configuration";
const CREDENTIAL: &str = "custody-credential-sentinel";
const PROVIDER: &str = "custody-provider-sentinel";
const KEY_ID: &str = "custody-key-id-sentinel";
const ENV_VAR: &str = "CUSTODY_ENV_SENTINEL";
const MISSING_CREDENTIAL: &str = "custody-missing-credential-sentinel";

#[derive(Clone, Debug, PartialEq)]
struct BootNotice {
    target: String,
    level: tracing::Level,
    fields: Vec<(String, String)>,
}

#[derive(Clone, Default)]
struct BootNotices(Arc<Mutex<Vec<BootNotice>>>);

impl tracing::Subscriber for BootNotices {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata.target() == "khive.boot"
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct Fields(Vec<(String, String)>);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0.push((field.name().to_owned(), format!("{value:?}")));
            }
        }
        let mut fields = Fields(Vec::new());
        event.record(&mut fields);
        self.0.lock().unwrap().push(BootNotice {
            target: event.metadata().target().to_owned(),
            level: *event.metadata().level(),
            fields: fields.0,
        });
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

fn capture(action: impl FnOnce()) -> Vec<BootNotice> {
    let notices = BootNotices::default();
    // A second live dispatcher prevents a concurrent subscriber-free test
    // from caching the warning callsite as disabled for this collector.
    let _second_dispatcher = tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default());
    tracing::subscriber::with_default(notices.clone(), action);
    let captured = notices.0.lock().unwrap().clone();
    captured
}

fn assert_notice(notices: &[BootNotice], message: &str) {
    assert_eq!(
        notices,
        &[BootNotice {
            target: "khive.boot".into(),
            level: tracing::Level::WARN,
            fields: vec![("message".into(), message.into())],
        }]
    );
    let all_fields = format!("{notices:?}");
    for sentinel in [CREDENTIAL, PROVIDER, KEY_ID, ENV_VAR, MISSING_CREDENTIAL] {
        assert!(
            !all_fields.contains(sentinel),
            "notice exposed a configuration field"
        );
    }
}

fn declaration() -> CredentialConfig {
    CredentialConfig {
        name: CREDENTIAL.into(),
        kind: CredentialKind::SigningKey,
        provider: PROVIDER.into(),
        env_var: None,
        header: None,
    }
}

fn ring() -> VisibilityReceiptConfig {
    VisibilityReceiptConfig {
        keys: vec![VisibilityReceiptKeyConfig {
            id: KEY_ID.into(),
            credential: CREDENTIAL.into(),
            encrypt: true,
        }],
    }
}

fn unusable_configs() -> [RuntimeConfig; 2] {
    let mut invalid_declaration = declaration();
    invalid_declaration.env_var = Some(ENV_VAR.into());
    let registry_failure = RuntimeConfig {
        credentials: vec![invalid_declaration],
        visibility_receipts: Some(ring()),
        ..RuntimeConfig::no_embeddings()
    };
    assert!(CredentialRegistry::new(registry_failure.credentials.clone()).is_err());

    let mut missing_reference = ring();
    missing_reference.keys[0].credential = MISSING_CREDENTIAL.into();
    let sealer_failure = RuntimeConfig {
        credentials: vec![declaration()],
        visibility_receipts: Some(missing_reference),
        ..RuntimeConfig::no_embeddings()
    };
    assert!(CredentialRegistry::new(sealer_failure.credentials.clone()).is_ok());
    assert!(sealer_failure
        .visibility_receipts
        .as_ref()
        .unwrap()
        .validate(&sealer_failure.credentials)
        .is_err());
    [registry_failure, sealer_failure]
}

struct NeverResolve(Arc<AtomicUsize>);

impl CredentialProvider for NeverResolve {
    fn resolve(&self, _: &str) -> Result<CredentialMaterial, CredentialError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("custody notice must not resolve a credential");
    }

    fn cache_lifetime(&self) -> CredentialCacheLifetime {
        CredentialCacheLifetime::NoCache
    }
}

fn configure(runtime: KhiveRuntime, resolutions: &Arc<AtomicUsize>) -> KhiveRuntime {
    let mut registry = CredentialRegistry::new(vec![declaration()]).unwrap();
    registry
        .register_provider(PROVIDER.into(), Arc::new(NeverResolve(resolutions.clone())))
        .unwrap();
    runtime
        .with_visibility_receipt_credentials(ring(), Arc::new(registry))
        .unwrap()
}

fn runtime(mut config: RuntimeConfig) -> KhiveRuntime {
    // no_embeddings() still inherits HOME's database and captured WAL policy.
    // Override both before opening anything; never mutate the process environment.
    config.db_path = None;
    config.wal_ceiling_bytes = 0;
    config.wal_ceiling_configured_bytes = 0;
    config.wal_ceiling_source = khive_runtime::WalCeilingSource::Default;
    config.wal_ceiling_env_raw = None;
    config.disable_embedding_models();
    config.packs = vec!["kg".into()];
    config.brain_profile = None;
    config.actor_id = None;
    assert!(config.db_path.is_none());
    KhiveRuntime::new(config).expect("isolated in-memory runtime")
}

fn builder(runtime: &KhiveRuntime, memory: bool) -> VerbRegistryBuilder {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    if memory {
        builder.register(MemoryPack::new(runtime.clone()));
    }
    builder
}

#[test]
fn absent_custody_warns_once_per_memory_pack_activation() {
    let runtime = runtime(RuntimeConfig::no_embeddings());
    for _ in 0..3 {
        let notices = capture(|| {
            let registry = builder(&runtime, true).build().unwrap();
            assert!(registry.has_verb("memory.remember"));
        });
        assert_notice(&notices, ABSENT_NOTICE);
    }
}

#[test]
fn programmatic_custody_is_quiet_and_never_resolved_at_activation() {
    let resolutions = Arc::new(AtomicUsize::new(0));
    // Open the store before capturing: a fresh database logs its own
    // migration line on khive.boot, which is not a custody notice.
    let runtime = runtime(RuntimeConfig::no_embeddings());
    let notices = capture(|| {
        let runtime = configure(runtime, &resolutions);
        assert_eq!(runtime.visibility_receipt_custody_notice(), None);
        let registry = builder(&runtime, true).build().unwrap();
        assert!(registry.has_verb("memory.remember"));
    });
    assert!(notices.is_empty(), "{notices:?}");
    assert_eq!(resolutions.load(Ordering::SeqCst), 0);
}

#[test]
fn unusable_custody_warns_without_exposing_configuration_fields() {
    for config in unusable_configs() {
        let runtime = runtime(config);
        assert_eq!(
            runtime.visibility_receipt_custody_notice(),
            Some(UNAVAILABLE_NOTICE)
        );
        let notices = capture(|| {
            let registry = builder(&runtime, true).build().unwrap();
            assert!(registry.has_verb("memory.remember"));
        });
        assert_notice(&notices, UNAVAILABLE_NOTICE);
    }
}

#[test]
fn metadata_and_no_memory_builds_are_quiet() {
    for config in std::iter::once(RuntimeConfig::no_embeddings()).chain(unusable_configs()) {
        let runtime = runtime(config);
        let notices = capture(|| {
            let metadata = builder(&runtime, true).build_metadata().unwrap();
            assert!(metadata.has_verb("memory.remember"));
            let registry = builder(&runtime, false).build().unwrap();
            assert!(registry.has_verb("stream.read"));
            assert!(!registry.has_verb("memory.remember"));
        });
        assert!(notices.is_empty(), "{notices:?}");
    }
}

#[test]
fn custody_notice_follows_the_memory_routed_runtime() {
    let resolutions = Arc::new(AtomicUsize::new(0));
    let absent = runtime(RuntimeConfig::no_embeddings());
    let configured = configure(runtime(RuntimeConfig::no_embeddings()), &resolutions);
    let names = vec!["kg".into(), "memory".into()];
    for (default, memory, warns) in [(&absent, &configured, false), (&configured, &absent, true)] {
        let notices = capture(|| {
            let runtimes = HashMap::from([("memory".into(), memory.clone())]);
            let mut builder = VerbRegistryBuilder::new();
            PackRegistry::register_packs_with_runtimes(&names, &runtimes, default, &mut builder)
                .expect("factories registered");
            let registry = builder.build().unwrap();
            assert!(registry.has_verb("memory.remember"));
        });
        if warns {
            assert_notice(&notices, ABSENT_NOTICE);
        } else {
            assert!(notices.is_empty(), "{notices:?}");
        }
    }
    assert_eq!(resolutions.load(Ordering::SeqCst), 0);
}
