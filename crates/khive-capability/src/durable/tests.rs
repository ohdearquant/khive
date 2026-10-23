use super::codec::{decode, encode};
use super::envelope::{check_alias, check_output_capacity, issuance_alias, seal_body, MAGIC};
use super::fixtures::*;
use super::*;
use khive_types::Timestamp;
use lion_core::{Capability, Kernel, Rights, SealedTag};
use uuid::Uuid;

fn ro(p: ObservationPermission) -> Vec<RightSpec> {
    vec![RightSpec::RecordObservation(ObservationScope {
        permissions: vec![p],
    })]
}
fn verify(
    keys: &SuppliedKeys,
    catalog: &ClosedCatalog,
    bytes: &[u8],
) -> EnvelopeResult<AuthenticatedEnvelope> {
    EnvelopeVerifier::new(keys, catalog).verify(bytes, Uuid::from_u128(101), Uuid::from_u128(102))
}
fn old_tag(body: &GrantBody, original: &[u8]) -> Vec<u8> {
    let body = encode(body).unwrap();
    let mut bytes = MAGIC.to_vec();
    bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
    bytes.extend(body);
    bytes.extend_from_slice(&original[original.len() - 32..]);
    bytes
}
#[test]
fn v1_permission_contract_is_closed() {
    let catalog = catalog();
    let alice = principal("alice");
    catalog
        .validate_rights(&ro(permission()), "research", &alice)
        .unwrap();
    let mut p = permission();
    p.entry = contract("relay", 6);
    p.producer_class = ProducerClass::Platform;
    p.binding = SubmissionBinding::PlatformRelay {
        source: source_binding(),
        response_contract: contract("response", 6),
    };
    catalog
        .validate_rights(&ro(p.clone()), "research", &alice)
        .unwrap();
    let accepted = p.clone();
    if let SubmissionBinding::PlatformRelay {
        response_contract, ..
    } = &mut p.binding
    {
        response_contract.digest.0[0] ^= 1;
    }
    assert!(catalog
        .validate_rights(&ro(p.clone()), "research", &alice)
        .is_err());
    assert!(!catalog.covers_rights(&ro(accepted), &ro(p)));
    let mut p = permission();
    p.binding = SubmissionBinding::ClaimTimePlatformBundle;
    assert_eq!(
        catalog.validate_rights(&ro(p), "research", &alice),
        Err(EnvelopeError::RightNotSupported(
            UnsupportedRight::ClaimTimePlatformBundleInM1
        ))
    );
    for channel in [SubmissionChannel::Sample, SubmissionChannel::Internal] {
        let mut p = permission();
        p.channel = channel;
        assert!(matches!(
            catalog.validate_rights(&ro(p), "research", &alice),
            Err(EnvelopeError::RightNotSupported(
                UnsupportedRight::InternalProducer
            ))
        ));
    }
    let mut p = permission();
    p.producer_class = ProducerClass::System;
    assert!(catalog.validate_rights(&ro(p), "research", &alice).is_err());
    for right in [
        RightSpec::Read,
        RightSpec::Create,
        RightSpec::Write,
        RightSpec::Delete,
        RightSpec::Send,
        RightSpec::GrantAdmin(AdminScope {
            namespace: "research".into(),
            target: AdminTarget::Read,
        }),
    ] {
        assert_eq!(
            catalog.validate_rights(&[right], "research", &alice),
            Err(EnvelopeError::RightNotSupported(
                UnsupportedRight::OrdinaryNamespace
            ))
        );
    }
    let mut p = permission();
    p.entry.digest.0[0] ^= 1;
    assert!(catalog.validate_rights(&ro(p), "research", &alice).is_err());
}
fn actor_report() -> ObservationPermission {
    let mut p = permission();
    p.entry = contract("report", 4);
    p.producer_class = ProducerClass::Actor;
    p.binding = SubmissionBinding::ActorReport {
        run: RunRef(Uuid::from_u128(8)),
        definition: ContractDigest([7; 32]),
        role: "reporter".into(),
    };
    p
}
#[test]
fn actor_report_pins_full_contract_and_source_is_not_producer() {
    let CatalogParts(a, b, mut c, d, e) = catalogs();
    let alternate = contract("report", 99);
    c.push(entry(alternate.clone(), ProducerClass::Actor));
    let catalog = ClosedCatalog::provision(a, b, c, d, e).unwrap();
    let rights = ro(actor_report());
    catalog
        .validate_rights(&rights, "research", &principal("alice"))
        .unwrap();
    assert!(catalog
        .validate_rights(&rights, "research", &principal("owner"))
        .is_err());
    let mut p = actor_report();
    p.entry = alternate;
    assert!(catalog
        .validate_rights(&ro(p), "research", &principal("alice"))
        .is_err());
    let mut grant = draft(9, "owner", "alice");
    grant.rights = rights;
    EnvelopeIssuer::new(&keys(), &catalog)
        .issue_from_bootstrap(&bootstrap(&grant), grant)
        .unwrap();
    let admin = RightSpec::GrantAdmin(AdminScope {
        namespace: "research".into(),
        target: AdminTarget::RecordObservation(ObservationScope {
            permissions: vec![actor_report()],
        }),
    });
    catalog
        .validate_rights(&[admin], "research", &principal("administrator"))
        .unwrap();
}
fn action() -> ObservationPermission {
    let mut p = actor_report();
    p.entry = contract("action", 5);
    p.binding = SubmissionBinding::ActionResult {
        run: RunRef(Uuid::from_u128(8)),
        definition: ContractDigest([7; 32]),
        occurrence: ActionOccurrence {
            visit: PhaseVisit {
                phase_id: "collect".into(),
                round: 1,
            },
            action_name: "measure".into(),
        },
        reporter: ReportingBinding::Role("reporter".into()),
        arguments: ExactArguments(vec![]),
    };
    p
}
#[test]
fn registration_defensively_rejects_ambiguous_action_occurrence() {
    let catalog = catalog();
    catalog
        .validate_rights(&ro(action()), "research", &principal("alice"))
        .unwrap();
    for round in [0, -1] {
        let mut p = action();
        if let SubmissionBinding::ActionResult { occurrence, .. } = &mut p.binding {
            occurrence.visit.round = round;
        }
        assert!(catalog
            .validate_rights(&ro(p), "research", &principal("alice"))
            .is_err());
    }
    let CatalogParts(a, b, c, mut d, e) = catalogs();
    let duplicate = d[0].actions[0].clone();
    d[0].actions.push(duplicate);
    let ambiguous = ClosedCatalog::provision(a, b, c, d, e).unwrap();
    assert!(ambiguous
        .validate_rights(&ro(action()), "research", &principal("alice"))
        .is_err());
    let CatalogParts(a, b, c, d, mut e) = catalogs();
    e[0].bindings.clear();
    let unbound = ClosedCatalog::provision(a, b, c, d, e).unwrap();
    assert!(unbound
        .validate_rights(&ro(action()), "research", &principal("initiator"))
        .is_err());
    let text = "abcdefab-0000-0000-0000-000000000008";
    assert!(RunRef::parse(text).is_ok());
    assert!(RunRef::parse(&text.to_uppercase()).is_err());
    assert!(RunRef::parse(&text.replace('-', "")).is_err());
}
#[test]
fn permission_coverage_preserves_complete_constraints() {
    let catalog = catalog();
    let parent = permission();
    let mut child = parent.clone();
    child.subject.selector = SubjectSelector::Exact(vec![ExactBytes("α".as_bytes().to_vec())]);
    if let SubmissionBinding::Instrument(source) = &mut child.binding {
        source.identities.truncate(1);
    }
    assert!(catalog.covers_rights(&ro(parent.clone()), &ro(child.clone())));
    let mut broad = child.clone();
    broad.subject.selector = SubjectSelector::AllSubjectsOfKind;
    assert!(!catalog.covers_rights(&ro(parent.clone()), &ro(broad)));
    let mut wrong = child.clone();
    wrong.policy_domain = "another".into();
    assert!(!catalog.covers_rights(&ro(parent.clone()), &ro(wrong)));
    let mut first = child.clone();
    let mut second = child.clone();
    first.policy_domain = "another".into();
    second.entry.digest.0[0] ^= 1;
    assert!(!catalog.covers_rights(
        &[RightSpec::RecordObservation(ObservationScope {
            permissions: vec![first, second]
        })],
        &ro(child)
    ));
    let mut requested = mixed(scope());
    if let RightSpec::GrantAdmin(admin) = &mut requested[1] {
        admin.namespace = "another".into();
    }
    assert!(!catalog.covers_rights(&mixed(scope()), &requested));
    let CatalogParts(a, b, mut c, d, e) = catalogs();
    c[0].authority_arguments = vec![
        ArgumentSpec {
            name: "mode".into(),
            kind: ArgumentType::Word,
        },
        ArgumentSpec {
            name: "count".into(),
            kind: ArgumentType::Integer,
        },
    ];
    let catalog = ClosedCatalog::provision(a, b, c, d, e).unwrap();
    let tuple = |n, word: &str| {
        ExactArguments(vec![
            ArgumentBinding {
                name: "mode".into(),
                value: ArgumentValue::Word(word.into()),
            },
            ArgumentBinding {
                name: "count".into(),
                value: ArgumentValue::Integer(n),
            },
        ])
    };
    let mut p = permission();
    if let SubmissionBinding::Instrument(s) = &mut p.binding {
        s.arguments = vec![tuple(1, "a"), tuple(2, "b")];
    }
    catalog
        .validate_rights(&ro(p.clone()), "research", &principal("alice"))
        .unwrap();
    let mut cross = p.clone();
    if let SubmissionBinding::Instrument(s) = &mut cross.binding {
        s.arguments = vec![tuple(1, "b")];
    }
    assert!(!catalog.covers_rights(&ro(p.clone()), &ro(cross)));
    if let SubmissionBinding::Instrument(s) = &mut p.binding {
        s.arguments[0].0[1].value = ArgumentValue::Word("1".into());
    }
    assert!(catalog
        .validate_rights(&ro(p), "research", &principal("alice"))
        .is_err());
}
#[test]
fn validity_uses_u64_half_open_microseconds() {
    let v = Validity {
        not_before: Timestamp::from_micros(10),
        expires_at: Some(Timestamp::from_micros(20)),
    };
    v.validate().unwrap();
    assert!(!v.contains(Timestamp::from_micros(9)));
    assert!(v.contains(Timestamp::from_micros(10)));
    assert!(!v.contains(Timestamp::from_micros(20)));
    for end in [9, 10] {
        assert!(Validity {
            expires_at: Some(Timestamp::from_micros(end)),
            ..v
        }
        .validate()
        .is_err());
    }
    let standing = Validity {
        not_before: Timestamp::from_micros(i64::MAX as u64 + 1),
        expires_at: None,
    };
    assert!(standing.contains(Timestamp::from_micros(u64::MAX)));
    assert_eq!(
        decode::<Validity>(&encode(&standing).unwrap()).unwrap(),
        standing
    );
}
#[test]
fn key_registry_refuses_invalid_provisioning_and_distinguishes_unavailable() {
    assert!(ProvisionedKey::new(key_ref(), [0; 32], [2; 32]).is_err());
    assert!(ProvisionedKey::new(key_ref(), [1; 32], [0; 32]).is_err());
    assert!(ProvisionedKey::new(key_ref(), [1; 32], [1; 32]).is_err());
    assert!(SuppliedKeys::provision(vec![
        ProvisionedKey::new(key_ref(), [1; 32], [2; 32]).unwrap(),
        ProvisionedKey::new(key_ref(), [3; 32], [4; 32]).unwrap()
    ])
    .is_err());
    let keys = keys();
    let catalog = catalog();
    let parent = parent(&keys, &catalog);
    let other = SuppliedKeys::provision(vec![ProvisionedKey::new(
        KeyRef {
            id: "other".into(),
            generation: 7,
        },
        [1; 32],
        [2; 32],
    )
    .unwrap()])
    .unwrap();
    assert!(matches!(
        verify(&other, &catalog, parent.as_bytes()),
        Err(EnvelopeError::Unavailable(_))
    ));
    let wrong = SuppliedKeys::provision(vec![
        ProvisionedKey::new(key_ref(), [3; 32], [4; 32]).unwrap()
    ])
    .unwrap();
    assert!(matches!(
        verify(&wrong, &catalog, parent.as_bytes()),
        Err(EnvelopeError::Reject(_))
    ));
    let debug = format!("{keys:?}");
    assert!(!debug.contains("17, 17"));
    assert!(!debug.contains("34, 34"));
}
#[test]
fn canonical_body_bytes_are_stable_and_complete() {
    // Independently encoded expected-format fixture, not an old/native capture.
    let expected = canonical_fixture_bytes();
    let body = fixed_body();
    assert_eq!(encode(&body).unwrap(), expected);
    assert_eq!(decode::<GrantBody>(&expected).unwrap(), body);
}
#[test]
fn decoder_rejects_noncanonical_and_oversized_data() {
    assert!(decode::<String>(&[0, 0, 0, 1, 255]).is_err());
    assert!(decode::<bool>(&[2]).is_err());
    assert!(decode::<ContractDigest>(&[2; 33]).is_err());
    assert!(decode::<Option<u64>>(&[2]).is_err());
    assert!(decode::<String>(&[255; 4]).is_err());
    assert!(decode::<RightSpec>(&[255]).is_err());
    assert_eq!(
        decode::<Vec<RightSpec>>(&257_u32.to_be_bytes()),
        Err(EnvelopeError::OverLimit {
            field: "collection",
            measured: 257,
            bound: 256
        })
    );
    let keys = keys();
    let catalog = catalog();
    let parent = parent(&keys, &catalog);
    let mut trailing = parent.as_bytes().to_vec();
    trailing.push(0);
    assert!(verify(&keys, &catalog, &trailing).is_err());
    assert_eq!(
        verify(&keys, &catalog, &vec![0; MAX_RECORD_BYTES + 1]),
        Err(EnvelopeError::OverLimit {
            field: "canonical record",
            measured: MAX_RECORD_BYTES + 1,
            bound: MAX_RECORD_BYTES
        })
    );
    for change in [0, 1] {
        let mut body = parent.body().clone();
        if change == 0 {
            body.grant.rights.push(body.grant.rights[0].clone());
        } else {
            body.grant.rights.reverse();
        }
        assert!(verify(&keys, &catalog, &seal_body(&body, &keys).unwrap()).is_err());
    }
}
#[test]
fn alias_ranges_refuse_capacity_before_overlap() {
    check_output_capacity((1_u128 << 126) - 1).unwrap();
    assert_eq!(
        check_output_capacity(1_u128 << 126),
        Err(EnvelopeError::IdSpaceExhausted)
    );
    let keys = keys();
    let catalog = catalog();
    let parent = parent(&keys, &catalog);
    let issuer = EnvelopeIssuer::new(&keys, &catalog);
    let mut candidate = draft(2, "alice", "bob");
    candidate.issuance_id = Uuid::from_u128(0);
    let child = issuer.delegate(&parent, candidate).unwrap();
    assert_ne!(child.body().native.id, ALIAS_START);
    for alias in [(1_u128 << 126) - 1, ALIAS_END, u128::MAX] {
        assert_eq!(check_alias(alias), Err(EnvelopeError::IdSpaceExhausted));
    }
    assert!(issuer
        .delegate(&parent, draft(2, "intruder", "bob"))
        .is_err());
    let mut broader = draft(2, "alice", "bob");
    broader.validity.not_before = Timestamp::from_micros(9);
    assert!(issuer.delegate(&parent, broader).is_err());
}
#[test]
fn native_and_extension_verification_are_both_required() {
    let keys = keys();
    let catalog = catalog();
    let parent = parent(&keys, &catalog);
    let mut body = parent.body().clone();
    body.native.id = 19;
    let n = &body.native;
    let native = Capability::new(
        n.id,
        n.holder,
        n.target,
        Rights::from_bits(n.rights),
        n.parent,
        n.epoch,
        SealedTag::from_bytes(n.signature),
    )
    .unwrap();
    assert!(Kernel::with_key([0x11; 32])
        .unwrap()
        .state()
        .kernel()
        .verify_cap_seal(&native));
    assert_eq!(
        verify(&keys, &catalog, &old_tag(&body, parent.as_bytes())),
        Err(EnvelopeError::Reject("extension MAC mismatch"))
    );
    body = parent.body().clone();
    body.native.signature[0] ^= 1;
    assert_eq!(
        verify(&keys, &catalog, &seal_body(&body, &keys).unwrap()),
        Err(EnvelopeError::Reject("native seal mismatch"))
    );
    for parent_id in [u128::MAX, 0] {
        body = parent.body().clone();
        body.native.parent = Some(parent_id);
        assert!(verify(&keys, &catalog, &seal_body(&body, &keys).unwrap()).is_err());
    }
    body = parent.body().clone();
    body.native.epoch = 1;
    assert!(verify(&keys, &catalog, &seal_body(&body, &keys).unwrap()).is_err());
}
#[test]
fn bound_field_mutations_refuse_after_cold_positive() {
    let keys = keys();
    let catalog = catalog();
    let parent = parent(&keys, &catalog);
    let child = EnvelopeIssuer::new(&keys, &catalog)
        .delegate(&parent, draft(2, "alice", "bob"))
        .unwrap();
    // Round-trip the serialized record first; mutations below retain its old tag.
    let cold = verify(&keys, &catalog, child.as_bytes()).unwrap();
    let assert_tamper = |name: &str, mutate: fn(&mut GrantBody)| {
        let mut body = cold.body().clone();
        mutate(&mut body);
        assert_eq!(
            verify(&keys, &catalog, &old_tag(&body, cold.as_bytes())),
            Err(EnvelopeError::Reject("extension MAC mismatch")),
            "{name}"
        );
    };
    assert_tamper("grant UUID", |b| b.grant.id = Uuid::from_u128(99));
    assert_tamper("store", |b| b.grant.store_id = Uuid::from_u128(99));
    assert_tamper("epoch", |b| b.grant.epoch_id = Uuid::from_u128(99));
    assert_tamper("root", |b| b.grant.root_anchor_id = Uuid::from_u128(99));
    assert_tamper("issuer", |b| b.grant.issuer.id.push('x'));
    assert_tamper("grantee", |b| b.grant.grantee.id.push('x'));
    assert_tamper("grantee kind", |b| b.grant.grantee.kind.push('x'));
    assert_tamper("grantee leg", |b| {
        b.grant.grantee.leg = Some("worker".into())
    });
    assert_tamper("namespace", |b| b.grant.namespace.push('x'));
    assert_tamper("not before", |b| {
        b.grant.validity.not_before = Timestamp::from_micros(11)
    });
    assert_tamper("expiry", |b| {
        b.grant.validity.expires_at = Some(Timestamp::from_micros(100))
    });
    assert_tamper("creation", |b| {
        b.grant.created_at = Timestamp::from_micros(12)
    });
    assert_tamper("action", |b| b.grant.action_id = Uuid::from_u128(99));
    assert_tamper("provenance", |b| {
        b.grant.operation = CreationOperation::Import;
        b.grant.configuration_source = Some("deployment".into());
    });
    assert_tamper("issuance", |b| b.grant.issuance_id = Uuid::from_u128(99));
    assert_tamper("carrier", |b| b.carrier_version = 2);
    assert_tamper("schema", |b| b.schema_version = 2);
    assert_tamper("profile", |b| b.profile_version = 2);
    assert_tamper("native ID", |b| b.native.id += 1);
    assert_tamper("native holder", |b| b.native.holder += 1);
    assert_tamper("native target", |b| b.native.target += 1);
    assert_tamper("native rights", |b| b.native.rights ^= 1);
    assert_tamper("native parent", |b| {
        b.native.parent = Some(ALIAS_START + 99)
    });
    assert_tamper("native epoch", |b| b.native.epoch = 1);
    assert_tamper("native signature", |b| b.native.signature[0] ^= 1);
    assert_tamper("context UUID", |b| b.context.id = Uuid::from_u128(99));
    assert_tamper("context holder", |b| b.context.holder += 1);
    assert_tamper("context principal", |b| {
        b.context.holder_principal.id.push('x')
    });
    assert_tamper("context target", |b| b.context.target += 1);
    assert_tamper("context namespace", |b| b.context.namespace.push('x'));
    assert_tamper("alias ID", |b| {
        if let ParentBinding::Grant { alias_id, .. } = &mut b.parent {
            *alias_id += 1;
        }
    });
    assert_tamper("parent UUID", |b| {
        if let ParentBinding::Grant { grant_id, .. } = &mut b.parent {
            *grant_id = Uuid::from_u128(99);
        }
    });
    assert_tamper("parent context", |b| {
        if let ParentBinding::Grant { issuance_id, .. } = &mut b.parent {
            *issuance_id = Uuid::from_u128(99);
        }
    });
    assert_tamper("parent original ID", |b| {
        if let ParentBinding::Grant {
            original_local_id, ..
        } = &mut b.parent
        {
            *original_local_id += 1;
        }
    });
    assert_tamper("parent native digest", |b| {
        if let ParentBinding::Grant { native_digest, .. } = &mut b.parent {
            native_digest.0[0] ^= 1;
        }
    });
    assert_tamper("admin namespace", |b| {
        if let RightSpec::GrantAdmin(a) = &mut b.grant.rights[1] {
            a.namespace.push('x');
        }
    });
    assert_tamper("right removed", |b| {
        b.grant.rights.pop();
    });
    assert_tamper("permission domain", |b| {
        permission_mut(b).policy_domain.push('x')
    });
    assert_tamper("entry name", |b| permission_mut(b).entry.name.push('x'));
    assert_tamper("entry digest", |b| permission_mut(b).entry.digest.0[0] ^= 1);
    assert_tamper("entry version", |b| permission_mut(b).entry.version += 1);
    assert_tamper("class", |b| {
        permission_mut(b).producer_class = ProducerClass::Platform
    });
    assert_tamper("channel", |b| {
        permission_mut(b).channel = SubmissionChannel::Sample
    });
    assert_tamper("subject contract", |b| {
        permission_mut(b).subject.kind.digest.0[0] ^= 1
    });
    assert_tamper("subject selector", |b| {
        permission_mut(b).subject.selector = SubjectSelector::AllSubjectsOfKind
    });
    assert_tamper("source contract", |b| {
        source_mut(b).contract.digest.0[0] ^= 1
    });
    assert_tamper("source identity", |b| {
        source_mut(b).identities[0].0.push(b'x')
    });
    assert_tamper("arguments", |b| {
        source_mut(b).arguments[0].0.push(ArgumentBinding {
            name: "new".into(),
            value: ArgumentValue::Bool(true),
        })
    });
    // A validly re-MACed alias-map transplant must still fail link verification.
    let mut altered = cold.body().clone();
    if let ParentBinding::Grant { native_digest, .. } = &mut altered.parent {
        native_digest.0[0] ^= 1;
    }
    let signed = verify(&keys, &catalog, &seal_body(&altered, &keys).unwrap()).unwrap();
    assert!(EnvelopeVerifier::new(&keys, &catalog)
        .verify_delegation_link(&signed, &parent)
        .is_err());
}
fn permission_mut(body: &mut GrantBody) -> &mut ObservationPermission {
    let RightSpec::RecordObservation(scope) = &mut body.grant.rights[0] else {
        panic!("fixture RO")
    };
    &mut scope.permissions[0]
}
fn source_mut(body: &mut GrantBody) -> &mut SourceBinding {
    let SubmissionBinding::Instrument(source) = &mut permission_mut(body).binding else {
        panic!("fixture instrument")
    };
    source
}
#[test]
fn exact_record_bound_uses_real_argument_content_and_reports_one_byte_over() {
    let CatalogParts(a, b, mut c, d, e) = catalogs();
    c[0].authority_arguments = vec![ArgumentSpec {
        name: "document".into(),
        kind: ArgumentType::String,
    }];
    let catalog = ClosedCatalog::provision(a, b, c, d, e).unwrap();
    let keys = keys();
    let mut p = permission();
    if let SubmissionBinding::Instrument(s) = &mut p.binding {
        s.arguments = vec![ExactArguments(vec![ArgumentBinding {
            name: "document".into(),
            value: ArgumentValue::String(String::new()),
        }])];
    }
    let mut grant = draft(1, "owner", "alice");
    grant.rights = mixed(ObservationScope {
        permissions: vec![p],
    });
    let issuer = EnvelopeIssuer::new(&keys, &catalog);
    let small = issuer
        .issue_from_bootstrap(&bootstrap(&grant), grant.clone())
        .unwrap();
    let text_length = MAX_RECORD_BYTES - small.as_bytes().len();
    let RightSpec::RecordObservation(scope) = &mut grant.rights[0] else {
        panic!("fixture RO")
    };
    let SubmissionBinding::Instrument(source) = &mut scope.permissions[0].binding else {
        panic!("fixture instrument")
    };
    source.arguments[0].0[0].value = ArgumentValue::String("x".repeat(text_length));
    let exact = issuer
        .issue_from_bootstrap(&bootstrap(&grant), grant.clone())
        .unwrap();
    assert_eq!(exact.as_bytes().len(), MAX_RECORD_BYTES);
    assert_eq!(exact.body().grant.rights.len(), 2);
    verify(&keys, &catalog, exact.as_bytes()).unwrap();
    let RightSpec::RecordObservation(scope) = &mut grant.rights[0] else {
        panic!("fixture RO")
    };
    let SubmissionBinding::Instrument(source) = &mut scope.permissions[0].binding else {
        panic!("fixture instrument")
    };
    let ArgumentValue::String(text) = &mut source.arguments[0].0[0].value else {
        panic!("fixture document")
    };
    text.push('x');
    assert_eq!(
        issuer.issue_from_bootstrap(&bootstrap(&grant), grant),
        Err(EnvelopeError::OverLimit {
            field: "canonical record",
            measured: MAX_RECORD_BYTES + 1,
            bound: MAX_RECORD_BYTES
        })
    );
    assert_eq!(MAX_REQUEST_BYTES, 1024 * 1024);
}
#[test]
fn canonical_set_order_is_encoded_bytes_and_semantic_duplicates_refuse() {
    // Length precedes UTF-8 bytes on the wire: "z" precedes "aa".
    let values = vec![ExactBytes(b"z".to_vec()), ExactBytes(b"aa".to_vec())];
    super::sorted_unique(&values, true).unwrap();
    assert!(super::sorted_unique(&[values[1].clone(), values[0].clone()], true).is_err());
    assert!(super::sorted_unique(&[values[0].clone(), values[0].clone()], true).is_err());
    assert_eq!(
        issuance_alias(Uuid::from_u128(0)),
        issuance_alias(Uuid::from_u128(1_u128 << 126))
    );
    // Aliases are local labels, not identities; full contexts remain distinct.
    let keys = keys();
    let catalog = catalog();
    let parent = parent(&keys, &catalog);
    let issuer = EnvelopeIssuer::new(&keys, &catalog);
    let verifier = EnvelopeVerifier::new(&keys, &catalog);
    let mut a = draft(2, "alice", "bob");
    a.issuance_id = Uuid::from_u128(0);
    let mut b = draft(3, "alice", "bob");
    b.issuance_id = Uuid::from_u128(1_u128 << 126);
    let a = issuer.delegate(&parent, a).unwrap();
    let b = issuer.delegate(&parent, b).unwrap();
    assert_ne!(a.body().context.id, b.body().context.id);
    assert_eq!(a.body().native.parent, b.body().native.parent);
    verifier.verify_delegation_link(&a, &parent).unwrap();
    verifier.verify_delegation_link(&b, &parent).unwrap();
}
#[test]
fn repository_coverage_uses_provisioned_immutable_membership() {
    let CatalogParts(mut a, b, c, d, e) = catalogs();
    let repo = |name: &str| RepositoryIdentity {
        forge: "forge".into(),
        immutable_id: ExactBytes(name.as_bytes().to_vec()),
    };
    a[0].repository_independent = false;
    a[0].repositories = vec![repo("r1"), repo("r2")];
    a[0].membership = vec![
        (ExactBytes("α".as_bytes().to_vec()), repo("r1")),
        (ExactBytes("β".as_bytes().to_vec()), repo("r2")),
    ];
    let catalog = ClosedCatalog::provision(a, b, c, d, e).unwrap();
    let mut parent = permission();
    parent.subject.selector = SubjectSelector::Repositories(vec![repo("r1"), repo("r2")]);
    catalog
        .validate_rights(&ro(parent.clone()), "research", &principal("alice"))
        .unwrap();
    let mut child = permission();
    child.subject.selector = SubjectSelector::Exact(vec![ExactBytes("α".as_bytes().to_vec())]);
    assert!(catalog.covers_rights(&ro(parent.clone()), &ro(child.clone())));
    child.subject.selector = SubjectSelector::Exact(vec![ExactBytes(b"unmapped".to_vec())]);
    assert!(!catalog.covers_rights(&ro(parent.clone()), &ro(child)));
    let mut unknown = parent.clone();
    unknown.subject.selector = SubjectSelector::Repositories(vec![repo("other")]);
    assert!(catalog
        .validate_rights(&ro(unknown), "research", &principal("alice"))
        .is_err());
    parent.subject.selector = SubjectSelector::AllSubjectsOfKind;
    assert!(catalog
        .validate_rights(&ro(parent), "research", &principal("alice"))
        .is_err());
}
#[test]
fn action_reporting_fallbacks_are_structural_and_explicit() {
    for phase_owner in [Some("reporter".to_string()), None] {
        let CatalogParts(a, b, c, mut d, e) = catalogs();
        d[0].actions[0].by = None;
        d[0].actions[0].phase_owner = phase_owner.clone();
        let catalog = ClosedCatalog::provision(a, b, c, d, e).unwrap();
        let mut p = action();
        let who = if phase_owner.is_some() {
            principal("alice")
        } else {
            principal("initiator")
        };
        if phase_owner.is_none() {
            if let SubmissionBinding::ActionResult { reporter, .. } = &mut p.binding {
                *reporter = ReportingBinding::Initiator(who.clone());
            }
        }
        catalog
            .validate_rights(&ro(p.clone()), "research", &who)
            .unwrap();
        assert!(catalog
            .validate_rights(&ro(p), "research", &principal("intruder"))
            .is_err());
    }
}
#[test]
fn selected_key_identity_and_generation_are_mac_bound() {
    let old = key_ref();
    let mut alternate = old.clone();
    alternate.id.push('2');
    let mut generation = old.clone();
    generation.generation += 1;
    let keys = SuppliedKeys::provision(
        [old, alternate.clone(), generation.clone()]
            .into_iter()
            .map(|reference| ProvisionedKey::new(reference, [0x11; 32], [0x22; 32]).unwrap())
            .collect(),
    )
    .unwrap();
    let catalog = catalog();
    let parent = parent(&keys, &catalog);
    for reference in [alternate, generation] {
        let mut body = parent.body().clone();
        body.grant.key = reference;
        assert_eq!(
            verify(&keys, &catalog, &old_tag(&body, parent.as_bytes())),
            Err(EnvelopeError::Reject("extension MAC mismatch"))
        );
    }
    let wrong_native =
        SuppliedKeys::provision(vec![
            ProvisionedKey::new(key_ref(), [0x33; 32], [0x22; 32]).unwrap()
        ])
        .unwrap();
    assert_eq!(
        verify(&wrong_native, &catalog, parent.as_bytes()),
        Err(EnvelopeError::Reject("native seal mismatch"))
    );
}
#[test]
fn legs_cannot_issue_or_hold_administration_and_parent_binding_is_exact() {
    let keys = keys();
    let catalog = catalog();
    let parent = parent(&keys, &catalog);
    let issuer = EnvelopeIssuer::new(&keys, &catalog);
    let mut grant = draft(2, "alice", "alice");
    grant.grantee.leg = Some("worker".into());
    grant.rights = ro(permission());
    issuer.delegate(&parent, grant.clone()).unwrap();
    grant.grantee.id = "bob".into();
    assert!(issuer.delegate(&parent, grant.clone()).is_err());
    grant.grantee.id = "alice".into();
    grant.rights = mixed(scope());
    assert!(issuer.delegate(&parent, grant.clone()).is_err());
    grant.rights = ro(permission());
    grant.issuer.leg = Some("worker".into());
    assert!(issuer.delegate(&parent, grant).is_err());
}
#[test]
fn bootstrap_and_actor_platform_binding_fields_are_mac_bound() {
    let keys = keys();
    let catalog = catalog();
    let parent = parent(&keys, &catalog);
    for which in 0..3 {
        let mut body = parent.body().clone();
        let ParentBinding::BootstrapSlot {
            slot,
            manifest_digest,
            anchor_local_id,
        } = &mut body.parent
        else {
            panic!("fixture bootstrap")
        };
        match which {
            0 => slot.push('x'),
            1 => manifest_digest.0[0] ^= 1,
            _ => *anchor_local_id -= 1,
        };
        assert_eq!(
            verify(&keys, &catalog, &old_tag(&body, parent.as_bytes())),
            Err(EnvelopeError::Reject("extension MAC mismatch"))
        );
    }
    let mut relay = permission();
    relay.entry = contract("relay", 6);
    relay.producer_class = ProducerClass::Platform;
    relay.binding = SubmissionBinding::PlatformRelay {
        source: source_binding(),
        response_contract: contract("response", 6),
    };
    for p in [actor_report(), action(), relay] {
        let mut grant = draft(1, "owner", "alice");
        grant.rights = ro(p);
        let original = EnvelopeIssuer::new(&keys, &catalog)
            .issue_from_bootstrap(&bootstrap(&grant), grant)
            .unwrap();
        for which in 0..3 {
            let mut body = original.body().clone();
            match &mut permission_mut(&mut body).binding {
                SubmissionBinding::ActorReport {
                    run,
                    definition,
                    role,
                } => match which {
                    0 => run.0 = Uuid::from_u128(99),
                    1 => definition.0[0] ^= 1,
                    _ => role.push('x'),
                },
                SubmissionBinding::ActionResult {
                    occurrence,
                    reporter,
                    arguments,
                    ..
                } => match which {
                    0 => occurrence.visit.round += 1,
                    1 => *reporter = ReportingBinding::Initiator(principal("another")),
                    _ => arguments.0.push(ArgumentBinding {
                        name: "extra".into(),
                        value: ArgumentValue::Duration(1),
                    }),
                },
                SubmissionBinding::PlatformRelay {
                    source,
                    response_contract,
                } => match which {
                    0 => response_contract.digest.0[0] ^= 1,
                    1 => response_contract.version += 1,
                    _ => source.contract.digest.0[0] ^= 1,
                },
                _ => panic!("fixture binding"),
            };
            assert_eq!(
                verify(&keys, &catalog, &old_tag(&body, original.as_bytes())),
                Err(EnvelopeError::Reject("extension MAC mismatch"))
            );
        }
    }
}
