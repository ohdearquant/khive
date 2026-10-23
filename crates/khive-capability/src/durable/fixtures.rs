use super::*;
use khive_types::Timestamp;
use uuid::Uuid;

pub(super) fn principal(id: &str) -> Principal {
    Principal {
        kind: "actor".into(),
        id: id.into(),
        leg: None,
    }
}
pub(super) fn contract(name: &str, byte: u8) -> ContractRef {
    ContractRef {
        name: name.into(),
        version: 1,
        digest: ContractDigest([byte; 32]),
    }
}
pub(super) fn key_ref() -> KeyRef {
    KeyRef {
        id: "fixture-generation".into(),
        generation: 7,
    }
}
pub(super) fn keys() -> SuppliedKeys {
    SuppliedKeys::provision(vec![
        ProvisionedKey::new(key_ref(), [0x11; 32], [0x22; 32]).unwrap()
    ])
    .unwrap()
}
pub(super) fn source_binding() -> SourceBinding {
    SourceBinding {
        contract: contract("source", 2),
        identities: vec![
            ExactBytes(b"sensor-a".to_vec()),
            ExactBytes(b"sensor-b".to_vec()),
        ],
        arguments: vec![ExactArguments(vec![])],
    }
}
pub(super) fn permission() -> ObservationPermission {
    ObservationPermission {
        policy_domain: "research".into(),
        entry: contract("instrument", 3),
        producer_class: ProducerClass::Instrument,
        channel: SubmissionChannel::Observe,
        subject: SubjectScope {
            kind: contract("subject", 1),
            selector: SubjectSelector::Exact(vec![
                ExactBytes("α".as_bytes().to_vec()),
                ExactBytes("β".as_bytes().to_vec()),
            ]),
        },
        binding: SubmissionBinding::Instrument(source_binding()),
    }
}
pub(super) fn scope() -> ObservationScope {
    ObservationScope {
        permissions: vec![permission()],
    }
}
pub(super) fn mixed(scope: ObservationScope) -> Vec<RightSpec> {
    vec![
        RightSpec::RecordObservation(scope.clone()),
        RightSpec::GrantAdmin(AdminScope {
            namespace: "research".into(),
            target: AdminTarget::RecordObservation(scope),
        }),
    ]
}
pub(super) fn entry(identity: ContractRef, class: ProducerClass) -> EntryContract {
    EntryContract {
        identity,
        class,
        subject_kind: contract("subject", 1),
        source_contract: (class != ProducerClass::Actor).then(|| contract("source", 2)),
        response_contract: (class == ProducerClass::Platform).then(|| contract("response", 6)),
        authority_arguments: vec![],
    }
}
pub(super) struct CatalogParts(
    pub Vec<SubjectContract>,
    pub Vec<SourceContract>,
    pub Vec<EntryContract>,
    pub Vec<DefinitionContract>,
    pub Vec<RunContract>,
);
pub(super) fn catalogs() -> CatalogParts {
    CatalogParts(
        vec![SubjectContract {
            identity: contract("subject", 1),
            reference_format: ReferenceFormat::Opaque,
            repository_independent: true,
            repositories: vec![],
            membership: vec![],
        }],
        vec![SourceContract {
            identity: contract("source", 2),
            reference_format: ReferenceFormat::Opaque,
            identities: source_binding().identities,
        }],
        vec![
            entry(contract("instrument", 3), ProducerClass::Instrument),
            entry(contract("report", 4), ProducerClass::Actor),
            entry(contract("action", 5), ProducerClass::Actor),
            entry(contract("relay", 6), ProducerClass::Platform),
        ],
        vec![DefinitionContract {
            digest: ContractDigest([7; 32]),
            reporting_roles: vec![(contract("report", 4), "reporter".into())],
            actions: vec![DeclaredAction {
                entry: contract("action", 5),
                phase_id: "collect".into(),
                action_name: "measure".into(),
                by: Some("reporter".into()),
                phase_owner: None,
            }],
        }],
        vec![RunContract {
            policy_domain: "research".into(),
            id: RunRef(Uuid::from_u128(8)),
            definition: ContractDigest([7; 32]),
            bindings: vec![("reporter".into(), principal("alice"))],
            initiating_principal: principal("initiator"),
            subject_kind: contract("subject", 1),
            subject: ExactBytes("α".as_bytes().to_vec()),
        }],
    )
}
pub(super) fn catalog() -> ClosedCatalog {
    let CatalogParts(a, b, c, d, e) = catalogs();
    ClosedCatalog::provision(a, b, c, d, e).unwrap()
}
pub(super) fn draft(id: u128, issuer: &str, grantee: &str) -> EnvelopeDraft {
    EnvelopeDraft {
        id: Uuid::from_u128(id),
        store_id: Uuid::from_u128(101),
        epoch_id: Uuid::from_u128(102),
        root_anchor_id: Uuid::from_u128(103),
        issuer: principal(issuer),
        grantee: principal(grantee),
        namespace: "research".into(),
        rights: mixed(scope()),
        validity: Validity {
            not_before: Timestamp::from_micros(10),
            expires_at: None,
        },
        created_at: Timestamp::from_micros(u64::MAX),
        assurance: IssuanceAssurance::ConfiguredLocal,
        operation: CreationOperation::Grant,
        action_id: Uuid::from_u128(id + 1000),
        configuration_source: None,
        issuance_id: Uuid::from_u128(id + 2000),
        key: key_ref(),
    }
}
pub(super) fn bootstrap(grant: &EnvelopeDraft) -> BootstrapSource {
    BootstrapSource {
        store_id: grant.store_id,
        epoch_id: grant.epoch_id,
        root_anchor_id: grant.root_anchor_id,
        owner: grant.issuer.clone(),
        namespace: grant.namespace.clone(),
        slot: "primary".into(),
        manifest_digest: ContractDigest([8; 32]),
        rights: grant.rights.clone(),
        validity: grant.validity,
    }
}
pub(super) fn parent(keys: &SuppliedKeys, catalog: &ClosedCatalog) -> AuthenticatedEnvelope {
    let grant = draft(1, "owner", "alice");
    EnvelopeIssuer::new(keys, catalog)
        .issue_from_bootstrap(&bootstrap(&grant), grant)
        .unwrap()
}
pub(super) fn fixed_body() -> GrantBody {
    let grant = draft(1, "owner", "alice");
    GrantBody {
        schema_version: 1,
        profile_version: 1,
        carrier_version: 1,
        context: IssuanceContext {
            id: grant.issuance_id,
            holder: 1,
            holder_principal: grant.grantee.clone(),
            target: 1,
            namespace: grant.namespace.clone(),
        },
        grant,
        parent: ParentBinding::BootstrapSlot {
            slot: "primary".into(),
            manifest_digest: ContractDigest([8; 32]),
            anchor_local_id: u128::MAX - 1,
        },
        native: NativeCapability {
            id: 0,
            holder: 1,
            target: 1,
            rights: 0x30,
            parent: Some(u128::MAX - 1),
            epoch: 0,
            signature: [0xa5; 32],
        },
    }
}

pub(super) fn canonical_fixture_bytes() -> Vec<u8> {
    let literal: &str = include!("fixtures/canonical_body_v1.rs");
    assert!(literal.len().is_multiple_of(2));
    literal
        .as_bytes()
        .chunks_exact(2)
        .map(|v| u8::from_str_radix(std::str::from_utf8(v).unwrap(), 16).unwrap())
        .collect()
}
