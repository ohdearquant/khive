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
        }
    }
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
    if metadata {
        builder.build_metadata().map(|_| ())
    } else {
        builder.build().map(|_| ())
    }
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
fn parser_compatible_public_names_build_and_parse_without_owner_restrictions() {
    const HANDLERS: &[HandlerDef] = &[
        handler("create", Visibility::Verb),
        handler("probe.read_2", Visibility::Verb),
        handler("stream.append", Visibility::Verb),
        handler("_Upper.Read2", Visibility::Verb),
        handler(
            "probe.identifier_longer_than_thirty_two_characters",
            Visibility::Verb,
        ),
    ];
    let pack = DeclarationPack {
        handlers: HANDLERS,
        ..DeclarationPack::named("probe")
    };
    for metadata in [false, true] {
        build(&[pack], metadata).unwrap();
    }
    for handler in HANDLERS {
        let parsed = khive_request::parse_request(&format!("{}()", handler.name)).unwrap();
        assert_eq!(parsed.ops.len(), 1);
        assert_eq!(parsed.ops[0].tool, handler.name);
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
