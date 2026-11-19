use super::*;
use khive_types::EdgeRelation;

#[derive(Clone, Copy)]
struct DeclarationPack {
    name: &'static str,
    notes: &'static [&'static str],
    entities: &'static [&'static str],
    handlers: &'static [HandlerDef],
    rules: &'static [EdgeEndpointRule],
    requires: &'static [&'static str],
    mounted: bool,
}

impl DeclarationPack {
    const fn named(name: &'static str) -> Self {
        Self {
            name,
            notes: &[],
            entities: &[],
            handlers: &[],
            rules: &[],
            requires: &[],
            mounted: false,
        }
    }
}

impl khive_types::Pack for DeclarationPack {
    const NAME: &'static str = "declaration_fixture";
    const NOTE_KINDS: &'static [&'static str] = &[];
    const ENTITY_KINDS: &'static [&'static str] = &[];
    const HANDLERS: &'static [HandlerDef] = &[];
}

#[async_trait]
impl PackRuntime for DeclarationPack {
    fn name(&self) -> &str {
        self.name
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        self.notes
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        self.entities
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        self.handlers
    }
    fn edge_rules(&self) -> &'static [EdgeEndpointRule] {
        self.rules
    }
    fn requires(&self) -> &'static [&'static str] {
        self.requires
    }
    fn mounted_namespace(&self) -> Option<&str> {
        self.mounted.then_some(self.name)
    }
    async fn dispatch(
        &self,
        _: &str,
        _: Value,
        _: &VerbRegistry,
        _: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        Ok(Value::Null)
    }
}

fn build(packs: &[DeclarationPack], metadata: bool) -> Result<(), RuntimeError> {
    let mut builder = VerbRegistryBuilder::new();
    for &pack in packs {
        builder.register_boxed(Box::new(pack));
    }
    finish_build(builder, metadata)
}

fn finish_build(builder: VerbRegistryBuilder, metadata: bool) -> Result<(), RuntimeError> {
    if metadata {
        builder.build_metadata().map(|_| ())
    } else {
        builder.build().map(|_| ())
    }
}

#[derive(Clone, Copy)]
enum Registration {
    Untrusted,
    Trusted,
    Boxed,
}

const REGISTRATIONS: [Registration; 3] = [
    Registration::Untrusted,
    Registration::Trusted,
    Registration::Boxed,
];

fn registered_builder(pack: DeclarationPack, registration: Registration) -> VerbRegistryBuilder {
    let mut builder = VerbRegistryBuilder::new();
    match registration {
        Registration::Untrusted => builder.register(pack),
        Registration::Trusted => builder.register_trusted(pack),
        Registration::Boxed => builder.register_boxed(Box::new(pack)),
    };
    builder
}

fn assert_invalid_declaration(error: RuntimeError, owner: &str, name: Option<&str>) -> String {
    let RuntimeError::InvalidInput(message) = error else {
        panic!("expected typed declaration refusal, got {error:?}");
    };
    assert!(message.contains(&format!("{owner:?}")), "{message}");
    if let Some(name) = name {
        assert!(message.contains(&format!("{name:?}")), "{message}");
    }
    message
}

const fn handler(name: &'static str, visibility: Visibility) -> HandlerDef {
    HandlerDef {
        name,
        description: "declaration probe",
        visibility,
        category: VerbCategory::Assertive,
        params: &[],
    }
}

const fn rule(source: EndpointKind, target: EndpointKind) -> EdgeEndpointRule {
    EdgeEndpointRule {
        relation: EdgeRelation::DependsOn,
        source,
        target,
    }
}

#[test]
fn invalid_public_handler_shapes_name_the_pack_and_handler() {
    const HANDLERS: &[HandlerDef] = &[
        handler("", Visibility::Verb),
        handler("bad name", Visibility::Verb),
        handler("bad-name", Visibility::Verb),
        handler("1bad", Visibility::Verb),
        handler(".read", Visibility::Verb),
        handler("probe.", Visibility::Verb),
        handler("probe.read.more", Visibility::Verb),
        handler("probe. read", Visibility::Verb),
        handler("préfix.read", Visibility::Verb),
    ];
    for handler in HANDLERS {
        for metadata in [false, true] {
            let pack = DeclarationPack {
                handlers: std::slice::from_ref(handler),
                ..DeclarationPack::named("probe")
            };
            let error = build(&[pack], metadata).unwrap_err().to_string();
            assert!(error.contains("invalid public handler"), "{error}");
            assert!(
                error.contains("\"probe\"") && error.contains(&format!("{:?}", handler.name)),
                "{error}"
            );
        }
    }
}

#[test]
fn parser_compatible_public_names_build_and_parse_under_their_owners() {
    const DECLARATIONS: &[(&str, HandlerDef)] = &[
        ("kg", handler("create", Visibility::Verb)),
        ("probe", handler("probe.read_2", Visibility::Verb)),
        ("kg", handler("stream.append", Visibility::Verb)),
        ("_Upper", handler("_Upper.Read2", Visibility::Verb)),
        (
            "probe",
            handler(
                "probe.identifier_longer_than_thirty_two_characters",
                Visibility::Verb,
            ),
        ),
        ("kg", handler("stream.read", Visibility::Verb)),
        ("kg", handler("stream.stat", Visibility::Verb)),
        ("kg", handler("stream.truncate", Visibility::Verb)),
        ("kg", handler("stream.batch", Visibility::Verb)),
        ("kg", handler("stream.future_2", Visibility::Verb)),
        ("Stream", handler("Stream.read", Visibility::Verb)),
    ];
    for (owner, handler) in DECLARATIONS {
        let pack = DeclarationPack {
            handlers: std::slice::from_ref(handler),
            ..DeclarationPack::named(owner)
        };
        for metadata in [false, true] {
            for registration in REGISTRATIONS {
                finish_build(registered_builder(pack, registration), metadata).unwrap();
            }
        }
        let parsed = khive_request::parse_request(&format!("{}()", handler.name)).unwrap();
        assert_eq!(parsed.ops.len(), 1);
        assert_eq!(parsed.ops[0].tool, handler.name);
    }
}

#[test]
fn public_ownership_refusals_apply_to_every_native_registration_path() {
    const DECLARATIONS: &[(&str, HandlerDef)] = &[
        ("probe", handler("create", Visibility::Verb)),
        ("probe", handler("list", Visibility::Verb)),
        ("kg", handler("unregistered", Visibility::Verb)),
        ("probe", handler("memory.recall", Visibility::Verb)),
        ("probe", handler("stream.read", Visibility::Verb)),
        ("probe", handler("stream.append", Visibility::Verb)),
        ("kg", handler("memory.recall", Visibility::Verb)),
        ("kg", handler("kg.proposal_cleanup", Visibility::Verb)),
        ("kg", handler("kg.create", Visibility::Verb)),
        ("kg", handler("other.read", Visibility::Verb)),
        ("Probe", handler("probe.read", Visibility::Verb)),
        ("probe", handler("Probe.read", Visibility::Verb)),
        (" probe", handler("probe.read", Visibility::Verb)),
        ("probe ", handler("probe.read", Visibility::Verb)),
        ("KG", handler("create", Visibility::Verb)),
        ("kg", handler("Create", Visibility::Verb)),
    ];
    for (owner, handler) in DECLARATIONS {
        let pack = DeclarationPack {
            handlers: std::slice::from_ref(handler),
            ..DeclarationPack::named(owner)
        };
        for metadata in [false, true] {
            for registration in REGISTRATIONS {
                let error = finish_build(registered_builder(pack, registration), metadata)
                    .expect_err("foreign public declaration must refuse construction");
                let message = assert_invalid_declaration(error, owner, Some(handler.name));
                assert!(message.contains("outside its namespace"), "{message}");
            }
        }
    }
}

#[test]
fn kg_cleanup_keeps_internal_visibility_without_a_public_exception() {
    const INTERNAL: &[HandlerDef] = &[handler("kg.proposal_cleanup", Visibility::Subhandler)];
    const PUBLIC: &[HandlerDef] = &[handler("kg.proposal_cleanup", Visibility::Verb)];
    for metadata in [false, true] {
        for registration in REGISTRATIONS {
            let internal = DeclarationPack {
                handlers: INTERNAL,
                ..DeclarationPack::named("kg")
            };
            finish_build(registered_builder(internal, registration), metadata)
                .expect("internal cleanup keeps its existing contract");
            let public = DeclarationPack {
                handlers: PUBLIC,
                ..DeclarationPack::named("kg")
            };
            let error = finish_build(registered_builder(public, registration), metadata)
                .expect_err("the internal CI exception must not admit public cleanup");
            assert_invalid_declaration(error, "kg", Some("kg.proposal_cleanup"));
        }
    }
}

#[test]
fn disabling_a_foreign_public_declaration_does_not_bypass_ownership() {
    const HANDLERS: &[HandlerDef] = &[handler("memory.recall", Visibility::Verb)];
    let pack = DeclarationPack {
        handlers: HANDLERS,
        ..DeclarationPack::named("probe")
    };
    for metadata in [false, true] {
        for registration in REGISTRATIONS {
            let mut builder = registered_builder(pack, registration);
            builder.with_disabled_verbs("probe", &["memory.recall".to_string()]);
            let error = finish_build(builder, metadata)
                .expect_err("disabled foreign declaration must still refuse construction");
            assert_invalid_declaration(error, "probe", Some("memory.recall"));

            let lawful = DeclarationPack {
                handlers: HANDLERS,
                ..DeclarationPack::named("memory")
            };
            let mut builder = registered_builder(lawful, registration);
            builder.with_disabled_verbs("memory", &["memory.recall".to_string()]);
            if metadata {
                let registry = builder
                    .build_metadata()
                    .expect("lawful disabled declaration");
                assert!(!registry.has_verb("memory.recall"));
            } else {
                let registry = builder.build().expect("lawful disabled declaration");
                assert!(!registry.has_verb("memory.recall"));
            }
        }
    }
}

#[test]
fn reserved_stream_owner_refuses_public_internal_and_empty_native_packs() {
    const DECLARATIONS: &[&[HandlerDef]] = &[
        &[],
        &[handler("stream.read", Visibility::Verb)],
        &[handler("stream.private", Visibility::Subhandler)],
    ];
    for &handlers in DECLARATIONS {
        let pack = DeclarationPack {
            handlers,
            ..DeclarationPack::named("stream")
        };
        for metadata in [false, true] {
            for registration in REGISTRATIONS {
                let error = finish_build(registered_builder(pack, registration), metadata)
                    .expect_err("stream is a reserved pack label");
                let message = assert_invalid_declaration(
                    error,
                    "stream",
                    handlers.first().map(|handler| handler.name),
                );
                assert!(message.contains("reserved owner label"), "{message}");
            }
        }
    }
}

#[test]
fn reserved_stream_mount_refuses_before_mutating_registration() {
    for metadata in [false, true] {
        let mut builder = VerbRegistryBuilder::new();
        let reserved = DeclarationPack {
            mounted: true,
            ..DeclarationPack::named("stream")
        };
        // This is an otherwise valid mount: matching namespace and owner, with
        // no native handlers. Assert the registration result before any build.
        let error = builder
            .register_mounted(Box::new(reserved))
            .map(|_| ())
            .expect_err("mounted stream namespace must refuse registration");
        let message = assert_invalid_declaration(error, "stream", None);
        assert!(message.contains("reserved mounted namespace"), "{message}");

        let valid = DeclarationPack {
            mounted: true,
            ..DeclarationPack::named("external")
        };
        builder
            .register_mounted(Box::new(valid))
            .expect("ordinary mounted namespace remains accepted");
        finish_build(builder, metadata).expect("refused mount must not remain in the builder");
    }
}

#[test]
fn kg_owned_stream_read_is_admitted_without_replay_safety() {
    const HANDLERS: &[HandlerDef] = &[
        handler("get", Visibility::Verb),
        handler("stream.read", Visibility::Verb),
    ];
    assert_eq!(
        crate::classify_operation("stream.read"),
        Some(crate::OperationAccess::Read)
    );
    for registration in REGISTRATIONS {
        let registry = registered_builder(
            DeclarationPack {
                handlers: HANDLERS,
                ..DeclarationPack::named("kg")
            },
            registration,
        )
        .build()
        .expect("kg owns stream public declarations");
        assert!(registry.has_verb("stream.read"));
        assert!(!registry.is_read_replay_safe("stream.read"));
        assert_eq!(
            registry.is_read_replay_safe("get"),
            !matches!(registration, Registration::Untrusted),
            "lawful ownership alone must not promote registration trust"
        );
    }
}

#[test]
fn same_named_subhandlers_keep_their_existing_internal_contract() {
    const HANDLERS: &[HandlerDef] = &[handler("internal-only.nested.name", Visibility::Subhandler)];
    let first = DeclarationPack {
        handlers: HANDLERS,
        ..DeclarationPack::named("first")
    };
    let second = DeclarationPack {
        handlers: HANDLERS,
        ..DeclarationPack::named("second")
    };
    for metadata in [false, true] {
        build(&[first, second], metadata).unwrap();
    }
}

#[test]
fn invalid_note_and_entity_kind_spellings_refuse_both_build_paths() {
    const KINDS: &[&[&str]] = &[
        &[""],
        &["Upper"],
        &["bad-kind"],
        &["1kind"],
        &["_kind"],
        &["kind "],
        &["naïve"],
    ];
    for &kinds in KINDS {
        for note in [false, true] {
            for metadata in [false, true] {
                let mut pack = DeclarationPack::named("probe");
                if note {
                    pack.notes = kinds;
                } else {
                    pack.entities = kinds;
                }
                let error = build(&[pack], metadata).unwrap_err().to_string();
                let substrate = if note { "note" } else { "entity" };
                assert!(
                    error.contains(&format!("invalid {substrate} kind")),
                    "{error}"
                );
                assert!(
                    error.contains("\"probe\"") && error.contains(&format!("{:?}", kinds[0])),
                    "{error}"
                );
            }
        }
    }
    let pack = DeclarationPack {
        notes: &["valid_note_2"],
        entities: &["valid_entity_2"],
        ..DeclarationPack::named("probe")
    };
    for metadata in [false, true] {
        build(&[pack], metadata).unwrap();
    }
}

#[test]
fn unknown_source_and_target_kinds_refuse_with_substrate_and_owner() {
    const RULES: &[(EdgeEndpointRule, &str, &str)] = &[
        (
            rule(
                EndpointKind::NoteOfKind("missing"),
                EndpointKind::NoteOfKind("known"),
            ),
            "source",
            "note",
        ),
        (
            rule(
                EndpointKind::NoteOfKind("known"),
                EndpointKind::NoteOfKind("missing"),
            ),
            "target",
            "note",
        ),
        (
            rule(
                EndpointKind::EntityOfKind("missing"),
                EndpointKind::EntityOfKind("concept"),
            ),
            "source",
            "entity",
        ),
        (
            rule(
                EndpointKind::EntityOfKind("concept"),
                EndpointKind::EntityOfKind("missing"),
            ),
            "target",
            "entity",
        ),
        (
            rule(
                EndpointKind::EntityOfType {
                    kind: "missing",
                    entity_type: "theorem",
                },
                EndpointKind::EntityOfKind("concept"),
            ),
            "source",
            "entity",
        ),
        (
            rule(
                EndpointKind::EntityOfKind("concept"),
                EndpointKind::EntityOfType {
                    kind: "missing",
                    entity_type: "theorem",
                },
            ),
            "target",
            "entity",
        ),
    ];
    for (rule, side, substrate) in RULES {
        for metadata in [false, true] {
            let pack = DeclarationPack {
                notes: &["known"],
                rules: std::slice::from_ref(rule),
                ..DeclarationPack::named("probe")
            };
            let error = build(&[pack], metadata).unwrap_err().to_string();
            assert!(
                error.contains(&format!("unregistered {side} {substrate} kind \"missing\"")),
                "{error}"
            );
            assert!(
                error.contains("\"probe\"") && error.contains("DependsOn"),
                "{error}"
            );
        }
    }
}

#[test]
fn endpoint_vocabulary_is_composed_before_rule_validation() {
    const RULES: &[EdgeEndpointRule] = &[
        rule(
            EndpointKind::NoteOfKind("dependency_note"),
            EndpointKind::EntityOfKind("dependency_entity"),
        ),
        rule(
            EndpointKind::EntityOfKind("person"),
            EndpointKind::EntityOfType {
                kind: "concept",
                entity_type: "theorem",
            },
        ),
    ];
    let consumer = DeclarationPack {
        rules: RULES,
        requires: &["provider"],
        ..DeclarationPack::named("consumer")
    };
    let provider = DeclarationPack {
        notes: &["dependency_note"],
        entities: &["dependency_entity"],
        ..DeclarationPack::named("provider")
    };
    for metadata in [false, true] {
        build(&[consumer, provider], metadata).unwrap();
    }
}

#[test]
fn endpoint_vocabularies_stay_separate_and_typed_stars_are_literal() {
    const RULES: &[EdgeEndpointRule] = &[
        rule(
            EndpointKind::NoteOfKind("concept"),
            EndpointKind::NoteOfKind("known_note"),
        ),
        rule(
            EndpointKind::EntityOfKind("known_note"),
            EndpointKind::EntityOfKind("concept"),
        ),
        rule(
            EndpointKind::EntityOfKind("*"),
            EndpointKind::EntityOfKind("concept"),
        ),
        rule(
            EndpointKind::NoteOfKind("*"),
            EndpointKind::NoteOfKind("known_note"),
        ),
    ];
    for rule in RULES {
        for metadata in [false, true] {
            let pack = DeclarationPack {
                notes: &["known_note"],
                rules: std::slice::from_ref(rule),
                ..DeclarationPack::named("probe")
            };
            let error = build(&[pack], metadata).unwrap_err().to_string();
            assert!(error.contains("unregistered source"), "{error}");
        }
    }
    assert!(khive_types::base_entity_rule_allows(
        "person",
        EdgeRelation::InstanceOf,
        "concept"
    ));
}

#[test]
fn repeated_note_kind_within_one_pack_names_kind_and_both_owner_positions() {
    let pack = DeclarationPack {
        notes: &["repeated_note", "repeated_note"],
        ..DeclarationPack::named("repeat_owner")
    };
    for metadata in [false, true] {
        let error = build(&[pack], metadata).unwrap_err().to_string();
        assert!(
            error.contains("duplicate note kind \"repeated_note\""),
            "{error}"
        );
        assert!(
            error.contains("claimed by both \"repeat_owner\" and \"repeat_owner\""),
            "{error}"
        );
    }
}
