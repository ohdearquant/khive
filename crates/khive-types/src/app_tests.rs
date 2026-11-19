use super::*;
use alloc::string::ToString;
use alloc::vec;

fn album() -> AppManifest {
    AppManifest {
        name: "album".into(),
        version: "0.1.0".into(),
        signer: "example-signer".into(),
        verbs: vec![
            AppVerb {
                name: "album.import".into(),
                rights: RightsClass::Create,
            },
            AppVerb {
                name: "album.find".into(),
                rights: RightsClass::Read,
            },
        ],
        entity_kinds: vec!["photo".into()],
        note_kinds: vec!["caption".into()],
        row_types: vec![
            RowTypeDecl {
                substrate: RowSubstrate::Entity,
                kind: "photo".into(),
                lifecycle: LifecycleClass::Corpus {
                    store: "album-features".into(),
                },
            },
            RowTypeDecl {
                substrate: RowSubstrate::Note,
                kind: "caption".into(),
                lifecycle: LifecycleClass::Primary,
            },
        ],
        capabilities: vec!["blob.read".into()],
        residency: Residency::Linked,
    }
}

fn gtd_names() -> OwnedNames<'static> {
    OwnedNames {
        owner: "gtd",
        verbs: vec!["gtd.assign", "gtd.next"],
        entity_kinds: vec![],
        note_kinds: vec!["task"],
    }
}

#[test]
fn a_consistent_manifest_validates_and_admits_against_core_packs() {
    let manifest = album();
    assert_eq!(manifest.validate(), Ok(()));
    assert_eq!(manifest.admit(&[gtd_names()]), Ok(()));
}

#[test]
fn undeclared_row_type_refuses_installation() {
    let mut manifest = album();
    manifest.note_kinds.push("comment".into());
    assert_eq!(
        manifest.validate(),
        Err(InstallRefusal::UndeclaredRowType {
            substrate: RowSubstrate::Note,
            kind: "comment".into(),
        })
    );
    let text = manifest.admit(&[]).unwrap_err().to_string();
    assert!(text.contains("note kind \"comment\""), "{text}");
    assert!(text.contains("no lifecycle class"), "{text}");
}

#[test]
fn a_declared_row_type_outside_the_slice_refuses() {
    let mut manifest = album();
    manifest.row_types.push(RowTypeDecl {
        substrate: RowSubstrate::Entity,
        kind: "camera".into(),
        lifecycle: LifecycleClass::Primary,
    });
    assert_eq!(
        manifest.validate(),
        Err(InstallRefusal::RowTypeOutsideSlice {
            substrate: RowSubstrate::Entity,
            kind: "camera".into(),
        })
    );
}

#[test]
fn a_corpus_row_type_names_its_store() {
    let mut manifest = album();
    manifest.row_types[0].lifecycle = LifecycleClass::Corpus {
        store: String::new(),
    };
    assert_eq!(
        manifest.validate(),
        Err(InstallRefusal::CorpusWithoutStore {
            substrate: RowSubstrate::Entity,
            kind: "photo".into(),
        })
    );
}

#[test]
fn empty_identity_and_duplicate_verbs_refuse() {
    let mut unsigned = album();
    unsigned.signer.clear();
    assert_eq!(
        unsigned.validate(),
        Err(InstallRefusal::EmptyIdentity { field: "signer" })
    );
    let mut doubled = album();
    doubled.verbs.push(AppVerb {
        name: "album.find".into(),
        rights: RightsClass::Write,
    });
    assert_eq!(
        doubled.validate(),
        Err(InstallRefusal::DuplicateVerb {
            verb: "album.find".into(),
        })
    );
}

#[test]
fn a_verb_name_already_owned_refuses_naming_both_owners() {
    let mut manifest = album();
    manifest.verbs.push(AppVerb {
        name: "gtd.assign".into(),
        rights: RightsClass::Create,
    });
    let refusal = manifest.admit(&[gtd_names()]).unwrap_err();
    assert_eq!(
        refusal,
        InstallRefusal::VerbOwned {
            verb: "gtd.assign".into(),
            owner: "gtd".into(),
            candidate: "album".into(),
        }
    );
    let text = refusal.to_string();
    assert!(text.contains("\"gtd\""), "{text}");
    assert!(text.contains("\"album\""), "{text}");
}

#[test]
fn a_kind_claim_already_owned_refuses_naming_both_owners() {
    let mut manifest = album();
    manifest.note_kinds.push("task".into());
    manifest.row_types.push(RowTypeDecl {
        substrate: RowSubstrate::Note,
        kind: "task".into(),
        lifecycle: LifecycleClass::Primary,
    });
    assert_eq!(manifest.validate(), Ok(()));
    let refusal = manifest.admit(&[gtd_names()]).unwrap_err();
    assert_eq!(
        refusal,
        InstallRefusal::KindOwned {
            substrate: RowSubstrate::Note,
            kind: "task".into(),
            owner: "gtd".into(),
            candidate: "album".into(),
        }
    );
    let text = refusal.to_string();
    assert!(
        text.contains("\"gtd\"") && text.contains("\"album\""),
        "{text}"
    );
}

#[test]
fn self_inconsistency_is_reported_before_any_conflict() {
    let mut manifest = album();
    manifest.note_kinds.push("comment".into());
    manifest.verbs.push(AppVerb {
        name: "gtd.assign".into(),
        rights: RightsClass::Create,
    });
    assert!(matches!(
        manifest.admit(&[gtd_names()]),
        Err(InstallRefusal::UndeclaredRowType { .. })
    ));
}

#[test]
fn non_conflicting_manifests_compose_in_either_order() {
    let first = album();
    let mut second = album();
    second.name = "ledger".into();
    second.verbs = vec![AppVerb {
        name: "ledger.post".into(),
        rights: RightsClass::Create,
    }];
    second.entity_kinds = vec!["account".into()];
    second.note_kinds = vec!["posting".into()];
    second.row_types = vec![
        RowTypeDecl {
            substrate: RowSubstrate::Entity,
            kind: "account".into(),
            lifecycle: LifecycleClass::Primary,
        },
        RowTypeDecl {
            substrate: RowSubstrate::Note,
            kind: "posting".into(),
            lifecycle: LifecycleClass::Primary,
        },
    ];
    assert_eq!(first.admit(&[gtd_names(), second.owned_names()]), Ok(()));
    assert_eq!(second.admit(&[gtd_names(), first.owned_names()]), Ok(()));
    assert_eq!(
        second.admit(&[first.owned_names(), second.owned_names()]),
        Err(InstallRefusal::VerbOwned {
            verb: "ledger.post".into(),
            owner: "ledger".into(),
            candidate: "ledger".into(),
        })
    );
}

#[test]
fn owned_names_mirror_the_manifest() {
    let manifest = album();
    let names = manifest.owned_names();
    assert_eq!(names.owner, "album");
    assert_eq!(names.verbs, vec!["album.import", "album.find"]);
    assert_eq!(names.entity_kinds, vec!["photo"]);
    assert_eq!(names.note_kinds, vec!["caption"]);
}

#[test]
fn rights_class_tokens_are_exact() {
    let tokens = RightsClass::ALL.map(RightsClass::as_str);
    assert_eq!(tokens, ["read", "create", "write", "delete", "send"]);
    for class in RightsClass::ALL {
        assert_eq!(RightsClass::from_token(class.as_str()), Some(class));
        assert_eq!(class.to_string(), class.as_str());
    }
    assert_eq!(RightsClass::from_token("commissive"), None);
    assert_eq!(RightsClass::from_token("Read"), None);
}

#[test]
fn residency_and_substrate_tokens_are_exact() {
    assert_eq!(Residency::None.as_str(), "none");
    assert_eq!(Residency::Linked.as_str(), "linked");
    assert_eq!(Residency::Process.as_str(), "process");
    assert_eq!(RowSubstrate::Note.as_str(), "note");
    assert_eq!(RowSubstrate::Entity.as_str(), "entity");
}

#[test]
fn lifecycle_tokens_resolve_with_representation_as_a_corpus_alias() {
    assert_eq!(
        LifecycleClass::from_token("primary", None),
        Some(LifecycleClass::Primary)
    );
    assert_eq!(
        LifecycleClass::from_token("corpus", Some("album-features")),
        Some(LifecycleClass::Corpus {
            store: "album-features".into(),
        })
    );
    assert_eq!(
        LifecycleClass::from_token("representation", Some("album-index")),
        Some(LifecycleClass::Corpus {
            store: "album-index".into(),
        })
    );
    assert_eq!(
        LifecycleClass::from_token("blob", None),
        Some(LifecycleClass::Blob)
    );
    assert_eq!(LifecycleClass::from_token("derived", None), None);
    assert_eq!(LifecycleClass::from_token("Primary", None), None);
    for (token, store) in [("primary", None), ("corpus", Some("s")), ("blob", None)] {
        let class = LifecycleClass::from_token(token, store).unwrap();
        assert_eq!(class.as_str(), token);
    }
    assert_eq!(
        LifecycleClass::from_token("representation", Some("s"))
            .unwrap()
            .as_str(),
        "corpus"
    );
}

#[test]
fn a_representation_without_a_store_refuses_like_a_corpus() {
    let mut manifest = album();
    manifest.row_types[0].lifecycle = LifecycleClass::from_token("representation", None).unwrap();
    assert_eq!(
        manifest.validate(),
        Err(InstallRefusal::CorpusWithoutStore {
            substrate: RowSubstrate::Entity,
            kind: "photo".into(),
        })
    );
}

#[test]
fn a_blob_row_type_validates_without_a_store() {
    let mut manifest = album();
    manifest.entity_kinds.push("original".into());
    manifest.row_types.push(RowTypeDecl {
        substrate: RowSubstrate::Entity,
        kind: "original".into(),
        lifecycle: LifecycleClass::Blob,
    });
    assert_eq!(manifest.validate(), Ok(()));
    assert_eq!(manifest.admit(&[gtd_names()]), Ok(()));
}
