//! Public declaration and registry boot controls; no write-enforcement assertions.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use khive_pack_comm::CommPack;
use khive_pack_kg::KgPack;
use khive_runtime::{
    KhiveRuntime, NamespaceToken, PackRuntime, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_types::{
    HandlerDef, NotePropertyPolicySpec, Pack, PropertyPolicy, VerbCategory, Visibility,
};
use serde_json::{json, Value};

const fn spec(
    kind: &'static str,
    key: &'static str,
    policy: PropertyPolicy,
) -> NotePropertyPolicySpec {
    NotePropertyPolicySpec { kind, key, policy }
}

// Literal acceptance oracle from the pre-declaration kind-specific protected-key table.
const COMM_EXPECTED: &[NotePropertyPolicySpec] = &[
    spec("message", "quarantined", PropertyPolicy::OwnerOnly),
    spec("message", "channel_kind", PropertyPolicy::OwnerOnly),
    spec("message", "channel_slug", PropertyPolicy::OwnerOnly),
    spec("message", "delivery_hold", PropertyPolicy::OwnerOnly),
    spec("message", "delivery_hold_reason", PropertyPolicy::OwnerOnly),
    spec("message", "delivery_hold_at", PropertyPolicy::OwnerOnly),
    spec(
        "message",
        "external_id_diagnostic_note_id",
        PropertyPolicy::OwnerOnly,
    ),
    spec("channel_health", "channel_kind", PropertyPolicy::OwnerOnly),
    spec("channel_health", "channel_slug", PropertyPolicy::OwnerOnly),
];

#[tokio::test]
async fn real_kg_comm_boot_exposes_all_nine_owned_property_policies() {
    let runtime = KhiveRuntime::memory().expect("memory runtime");
    let comm = CommPack::new(runtime.clone());
    let erased: &dyn PackRuntime = &comm;
    assert_eq!(<CommPack as Pack>::NOTE_PROPERTY_POLICIES, COMM_EXPECTED);
    assert_eq!(erased.note_property_policies(), COMM_EXPECTED);
    for declaration in COMM_EXPECTED {
        assert!(erased.note_kinds().contains(&declaration.kind));
    }
    let mut builder = VerbRegistryBuilder::new();
    // Exercise the real dependency order, not registration order.
    builder.register(comm);
    builder.register(KgPack::new(runtime));
    let registry = builder.build().expect("real KG+Comm registry");
    assert_eq!(registry.all_note_property_policies(), COMM_EXPECTED);
}

#[derive(Default)]
struct Observations {
    policy_calls: AtomicUsize,
    activations: AtomicUsize,
    calls_at_activation: AtomicUsize,
    dispatches: AtomicUsize,
    switched: AtomicBool,
}

// Existing-style pack: deliberately implements neither new metadata member.
struct LegacyPack;
impl Pack for LegacyPack {
    const NAME: &'static str = "legacy";
    const NOTE_KINDS: &'static [&'static str] = &["legacy_kind"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[];
}
#[async_trait]
impl PackRuntime for LegacyPack {
    khive_runtime::pack_runtime_metadata!();
    async fn dispatch(
        &self,
        _verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        panic!("verbless legacy pack must not dispatch")
    }
}

struct BasePack(Arc<Observations>);
impl Pack for BasePack {
    const NAME: &'static str = "policy_base";
    const NOTE_KINDS: &'static [&'static str] = &["base_kind"];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[];
    const NOTE_PROPERTY_POLICIES: &'static [NotePropertyPolicySpec] =
        &[spec("base_kind", "owner", PropertyPolicy::OwnerOnly)];
}
#[async_trait]
impl PackRuntime for BasePack {
    khive_runtime::pack_runtime_metadata!();
    fn note_property_policies(&self) -> &'static [NotePropertyPolicySpec] {
        <Self as Pack>::NOTE_PROPERTY_POLICIES
    }
    fn validate_config(&self) -> Result<(), RuntimeError> {
        self.0.activations.fetch_add(1, Ordering::SeqCst);
        self.0
            .calls_at_activation
            .store(self.0.policy_calls.load(Ordering::SeqCst), Ordering::SeqCst);
        Ok(())
    }
    async fn dispatch(
        &self,
        _verb: &str,
        _params: Value,
        _registry: &VerbRegistry,
        _token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        panic!("verbless base pack must not dispatch")
    }
}

const NORMAL: &[NotePropertyPolicySpec] = &[
    spec("probe_kind", "derived", PropertyPolicy::Derived),
    spec("probe_kind", "owner", PropertyPolicy::OwnerOnly),
    spec("peer_kind", "owner", PropertyPolicy::Derived),
];
const REPLACEMENT: &[NotePropertyPolicySpec] = &[spec(
    "probe_kind",
    "changed_after_boot",
    PropertyPolicy::OwnerOnly,
)];

// Each instantiation supplies matching static Pack/runtime metadata. Case 0 can
// deliberately change its runtime slice after boot to test snapshot isolation.
struct ProbePack<const CASE: u8>(Arc<Observations>);
impl<const CASE: u8> Pack for ProbePack<CASE> {
    const NAME: &'static str = "policy_probe";
    const NOTE_KINDS: &'static [&'static str] = if CASE == 5 {
        &["base_kind"]
    } else {
        &["probe_kind", "peer_kind"]
    };
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const REQUIRES: &'static [&'static str] = &["policy_base"];
    const HANDLERS: &'static [HandlerDef] = &[HandlerDef {
        name: "policy_probe.echo",
        description: "Return a fixture payload with its authorized namespace",
        visibility: Visibility::Verb,
        category: VerbCategory::Assertive,
        params: &[],
    }];
    const NOTE_PROPERTY_POLICIES: &'static [NotePropertyPolicySpec] = match CASE {
        0 => NORMAL,
        1 => &[spec("base_kind", "foreign_key", PropertyPolicy::OwnerOnly)],
        2 | 5 => &[spec("missing_kind", "unknown_key", PropertyPolicy::Derived)],
        3 => &[
            spec("probe_kind", "duplicate", PropertyPolicy::OwnerOnly),
            spec("probe_kind", "duplicate", PropertyPolicy::OwnerOnly),
        ],
        4 => &[
            spec("probe_kind", "duplicate", PropertyPolicy::Derived),
            spec("probe_kind", "duplicate", PropertyPolicy::OwnerOnly),
        ],
        6 => &[
            spec("probe_kind", "", PropertyPolicy::Derived),
            spec("probe_kind", "Key", PropertyPolicy::Derived),
            spec("probe_kind", "key", PropertyPolicy::OwnerOnly),
            spec("probe_kind", " key ", PropertyPolicy::OwnerOnly),
        ],
        _ => &[],
    };
}
#[async_trait]
impl<const CASE: u8> PackRuntime for ProbePack<CASE> {
    khive_runtime::pack_runtime_metadata!();
    fn note_property_policies(&self) -> &'static [NotePropertyPolicySpec] {
        self.0.policy_calls.fetch_add(1, Ordering::SeqCst);
        if CASE == 0 && self.0.switched.load(Ordering::SeqCst) {
            REPLACEMENT
        } else {
            <Self as Pack>::NOTE_PROPERTY_POLICIES
        }
    }
    fn validate_config(&self) -> Result<(), RuntimeError> {
        self.0.activations.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn dispatch(
        &self,
        _verb: &str,
        params: Value,
        _registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        self.0.dispatches.fetch_add(1, Ordering::SeqCst);
        Ok(json!({"payload": params["payload"], "namespace": token.namespace().as_str()}))
    }
}

fn builder<const CASE: u8>(observations: &Arc<Observations>) -> VerbRegistryBuilder {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(ProbePack::<CASE>(observations.clone()));
    builder.register(BasePack(observations.clone()));
    builder
}

fn invalid_message<T>(result: Result<T, RuntimeError>) -> String {
    match result {
        Err(RuntimeError::InvalidInput(message)) => message,
        Err(other) => panic!("wrong error type: {other}"),
        Ok(_) => panic!("malformed declarations must refuse registry construction"),
    }
}

fn assert_invalid_before_activation<const CASE: u8>(kind: &str, key: &str, reason: &str) {
    for metadata_only in [false, true] {
        let observations = Arc::new(Observations::default());
        let builder = builder::<CASE>(&observations);
        let message = if metadata_only {
            invalid_message(builder.build_metadata())
        } else {
            invalid_message(builder.build())
        };
        for expected in ["policy_probe", kind, key, reason] {
            assert!(
                message.contains(expected),
                "{message:?} must identify {expected:?}"
            );
        }
        assert_eq!(observations.policy_calls.load(Ordering::SeqCst), 1);
        assert_eq!(observations.activations.load(Ordering::SeqCst), 0);
        assert_eq!(observations.dispatches.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn default_members_and_empty_registry_contribute_no_policies() {
    let empty = VerbRegistryBuilder::new().build().expect("empty registry");
    assert!(empty.all_note_property_policies().is_empty());
    assert!(<LegacyPack as Pack>::NOTE_PROPERTY_POLICIES.is_empty());
    let legacy: &dyn PackRuntime = &LegacyPack;
    assert!(legacy.note_property_policies().is_empty());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(LegacyPack);
    assert!(builder
        .build()
        .expect("legacy pack")
        .all_note_property_policies()
        .is_empty());
}

#[test]
fn dependency_order_and_within_pack_order_preserve_both_policy_variants() {
    let observations = Arc::new(Observations::default());
    let registry = builder::<0>(&observations).build().expect("valid policies");
    assert_eq!(
        registry.all_note_property_policies(),
        vec![
            spec("base_kind", "owner", PropertyPolicy::OwnerOnly),
            spec("probe_kind", "derived", PropertyPolicy::Derived),
            spec("probe_kind", "owner", PropertyPolicy::OwnerOnly),
            spec("peer_kind", "owner", PropertyPolicy::Derived),
        ]
    );
    assert_eq!(observations.activations.load(Ordering::SeqCst), 2);
    // Even the first dependency's activation observes the completed collection.
    assert_eq!(observations.calls_at_activation.load(Ordering::SeqCst), 1);
    assert_eq!(observations.dispatches.load(Ordering::SeqCst), 0);
}

#[test]
fn boot_snapshot_is_collected_once_and_isolated_from_packs_and_returned_vectors() {
    let observations = Arc::new(Observations::default());
    let registry = builder::<0>(&observations).build().expect("valid policies");
    assert_eq!(observations.policy_calls.load(Ordering::SeqCst), 1);
    observations.switched.store(true, Ordering::SeqCst);
    let cloned = registry.clone();
    let expected = vec![
        spec("base_kind", "owner", PropertyPolicy::OwnerOnly),
        spec("probe_kind", "derived", PropertyPolicy::Derived),
        spec("probe_kind", "owner", PropertyPolicy::OwnerOnly),
        spec("peer_kind", "owner", PropertyPolicy::Derived),
    ];
    for _ in 0..3 {
        let mut detached = registry.all_note_property_policies();
        assert_eq!(detached, expected);
        detached.clear();
        detached.extend_from_slice(REPLACEMENT);
        assert_eq!(cloned.all_note_property_policies(), expected);
    }
    drop(registry);
    assert_eq!(cloned.all_note_property_policies(), expected);
    assert_eq!(observations.policy_calls.load(Ordering::SeqCst), 1);
    assert_eq!(observations.dispatches.load(Ordering::SeqCst), 0);
}

#[test]
fn another_loaded_packs_kind_is_not_owned_by_the_declarer() {
    assert_invalid_before_activation::<1>("base_kind", "foreign_key", "unowned");
}

#[test]
fn an_unknown_kind_refuses_both_build_paths() {
    assert_invalid_before_activation::<2>("missing_kind", "unknown_key", "unowned");
}

#[test]
fn duplicate_equal_policy_refuses_both_build_paths() {
    assert_invalid_before_activation::<3>("probe_kind", "duplicate", "duplicate");
}

#[test]
fn duplicate_conflicting_policy_refuses_both_build_paths() {
    assert_invalid_before_activation::<4>("probe_kind", "duplicate", "duplicate");
}

#[test]
fn existing_duplicate_kind_validation_precedes_policy_collection() {
    for metadata_only in [false, true] {
        let observations = Arc::new(Observations::default());
        let builder = builder::<5>(&observations);
        let message = if metadata_only {
            invalid_message(builder.build_metadata())
        } else {
            invalid_message(builder.build())
        };
        assert_eq!(message, "duplicate note kind \"base_kind\": claimed by both \"policy_base\" and \"policy_probe\"");
        assert_eq!(observations.policy_calls.load(Ordering::SeqCst), 0);
        assert_eq!(observations.activations.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn property_keys_are_exact_without_an_invented_grammar() {
    let observations = Arc::new(Observations::default());
    let registry = builder::<6>(&observations).build().expect("exact keys");
    assert_eq!(
        registry.all_note_property_policies(),
        vec![
            spec("base_kind", "owner", PropertyPolicy::OwnerOnly),
            spec("probe_kind", "", PropertyPolicy::Derived),
            spec("probe_kind", "Key", PropertyPolicy::Derived),
            spec("probe_kind", "key", PropertyPolicy::OwnerOnly),
            spec("probe_kind", " key ", PropertyPolicy::OwnerOnly),
        ]
    );
}

#[test]
fn metadata_build_collects_once_without_configuration_activation() {
    let observations = Arc::new(Observations::default());
    let _metadata = builder::<0>(&observations)
        .build_metadata()
        .expect("metadata registry");
    assert_eq!(observations.policy_calls.load(Ordering::SeqCst), 1);
    assert_eq!(observations.activations.load(Ordering::SeqCst), 0);
    assert_eq!(observations.dispatches.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn discovery_does_not_execute_or_gate_the_existing_handler() {
    let observations = Arc::new(Observations::default());
    let registry = builder::<0>(&observations)
        .build()
        .expect("serving registry");
    let _ = registry.all_note_property_policies();
    assert_eq!(observations.dispatches.load(Ordering::SeqCst), 0);
    let payload = json!({"derived": "caller value", "owner": 7});
    let result = registry
        .dispatch(
            "policy_probe.echo",
            json!({
                "namespace": "policy-tests", "payload": payload,
            }),
        )
        .await
        .expect("ordinary handler dispatch");
    assert_eq!(
        result,
        json!({"namespace": "policy-tests", "payload": payload})
    );
    assert_eq!(observations.dispatches.load(Ordering::SeqCst), 1);
    assert_eq!(observations.policy_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn existing_disabled_verb_validation_precedes_policy_collection() {
    for metadata_only in [false, true] {
        let observations = Arc::new(Observations::default());
        let mut builder = builder::<2>(&observations);
        builder.with_disabled_verbs("policy_probe", &["policy_probe.absent".to_owned()]);
        let message = if metadata_only {
            invalid_message(builder.build_metadata())
        } else {
            invalid_message(builder.build())
        };
        assert!(message.starts_with("packs.policy_probe.verbs_disabled:"));
        assert!(message.contains("policy_probe.absent"));
        assert_eq!(observations.policy_calls.load(Ordering::SeqCst), 0);
        assert_eq!(observations.activations.load(Ordering::SeqCst), 0);
    }
}
