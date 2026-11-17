use khive_subject::{
    DependencyKind, EntityIntent, ExtractError, Extractor, FormalExtractor, LinkIntent,
    OntologySpec, RawDecl,
};
use khive_types::{EdgeRelation, EntityKind};

fn declaration(name: &str, kind: &str, type_expr: &str, refs: &[&str]) -> RawDecl {
    RawDecl {
        name: name.into(),
        kind: kind.into(),
        type_expr: type_expr.into(),
        refs: refs.iter().map(|value| (*value).into()).collect(),
        doc: None,
        module: "Mathlib.Test.Subject".into(),
    }
}

fn projection(parent: &str, head: &str, type_expr: &str) -> Vec<RawDecl> {
    vec![
        declaration(parent, "inductive", "Type", &[]),
        declaration(head, "inductive", "Type", &[]),
        declaration(
            &format!("{parent}.to{}", head.rsplit('.').next().unwrap()),
            "def",
            type_expr,
            &[parent, head],
        ),
    ]
}

fn edge(source: &str, target: &str, relation: EdgeRelation) -> LinkIntent {
    LinkIntent {
        source: source.into(),
        target: target.into(),
        relation,
        dependency_kind: (relation == EdgeRelation::DependsOn).then_some(DependencyKind::Runtime),
    }
}

fn extract(rows: &[RawDecl]) -> khive_subject::OperationPlan {
    FormalExtractor
        .extract(rows, &OntologySpec::formal_math().unwrap())
        .unwrap()
}

#[test]
fn three_declarations_produce_literal_typed_entities_and_dependency_intents() {
    let mut rows = vec![
        declaration(
            "Proof",
            "theorem",
            "∀ n, n = n",
            &["Helper", "Rule", "Helper", "outside"],
        ),
        declaration("Helper", "def", "Nat → Nat", &["Rule"]),
        declaration("Rule", "axiom", "Prop", &[]),
    ];
    rows[0].doc = Some("  定理\nλ  ".into());
    rows[1].doc = Some(String::new());
    let plan = extract(&rows);
    assert_eq!(
        plan.entities,
        vec![
            EntityIntent {
                name: "Proof".into(),
                base_kind: EntityKind::Concept,
                entity_type: "theorem".into(),
                type_expr: "∀ n, n = n".into(),
                doc: Some("  定理\nλ  ".into()),
                module: "Mathlib.Test.Subject".into()
            },
            EntityIntent {
                name: "Helper".into(),
                base_kind: EntityKind::Concept,
                entity_type: "definition".into(),
                type_expr: "Nat → Nat".into(),
                doc: Some(String::new()),
                module: "Mathlib.Test.Subject".into()
            },
            EntityIntent {
                name: "Rule".into(),
                base_kind: EntityKind::Concept,
                entity_type: "axiom".into(),
                type_expr: "Prop".into(),
                doc: None,
                module: "Mathlib.Test.Subject".into()
            },
        ],
        "exact input-ordered typed entities; no formula entity or synthesized doc"
    );
    assert_eq!(
        plan.edges,
        vec![
            edge("Proof", "Helper", EdgeRelation::DependsOn),
            edge("Proof", "Rule", EdgeRelation::DependsOn),
            edge("Helper", "Rule", EdgeRelation::DependsOn)
        ],
        "all and only first-seen in-corpus references point consumer to dependency"
    );
}

#[test]
fn structure_and_class_projections_require_all_seven_conditions() {
    for (parent, head, ty) in [
        ("Point3", "Point2", "(self : Point3) → Point2"),
        (
            "CommMonoid",
            "Monoid",
            "{M : Type u} → [self : CommMonoid M] → Monoid M",
        ),
    ] {
        let rows = projection(parent, head, ty);
        assert_eq!(
            extract(&rows).edges,
            vec![
                edge(&rows[2].name, parent, EdgeRelation::DependsOn),
                edge(&rows[2].name, head, EdgeRelation::DependsOn),
                edge(parent, head, EdgeRelation::Extends),
            ],
            "generated projection must emit parent-to-head extends, with references retained"
        );
    }
    let original = projection("P", "H", "(self : P) → H");
    for condition in 1..=7 {
        let mut rows = original.clone();
        match condition {
            1 => rows[2].name = "P.convertH".into(),
            2 => rows[2].refs = vec!["P".into()],
            3 => {
                rows.remove(0);
            }
            4 => rows[2].name = "P.toOther".into(),
            5 => rows[0].kind = "def".into(),
            6 => rows[2].kind = "theorem".into(),
            7 => rows[2].type_expr = "P → H".into(),
            _ => unreachable!(),
        }
        let plan = extract(&rows);
        assert!(
            plan.edges
                .iter()
                .all(|intent| intent.relation != EdgeRelation::Extends),
            "failed condition {condition} must prevent extends"
        );
        assert!(
            plan.edges
                .iter()
                .any(|intent| intent.relation == EdgeRelation::DependsOn),
            "failed condition {condition} must retain in-corpus reference evidence"
        );
    }
}

#[test]
fn codomain_and_immediate_self_parser_accepts_literal_supported_forms() {
    for ty in [
        "(self : P) -> H",
        "∀ (self : P), H",
        "forall (self : P), H",
        "∀ self : P, H",
        "{α : Type u} → [self : P α] → H α",
        "(self : P.{u} α) → H.{v,u} α",
        "((self : P) → (H (Nat × Nat)))",
        "(self : P (Nat → Nat)) → @H (Nat × Nat)",
    ] {
        let plan = extract(&projection("P", "H", ty));
        assert!(
            plan.edges.contains(&edge("P", "H", EdgeRelation::Extends)),
            "supported complete type must yield projection: {ty}"
        );
    }
    for (parent, head, ty) in [
        ("CategoryTheory.Bicategory", "CategoryTheory.CategoryStruct", "{B : Type u} → [self : CategoryTheory.Bicategory B] → CategoryTheory.CategoryStruct.{v, u} B"),
        ("CategoryTheory.Category", "CategoryTheory.CategoryStruct", "{obj : Type u} → [self : CategoryTheory.Category.{v, u} obj] → CategoryTheory.CategoryStruct.{v, u} obj"),
        ("CategoryTheory.Groupoid", "CategoryTheory.Category", "{obj : Type u} → [self : CategoryTheory.Groupoid obj] → CategoryTheory.Category.{v, u} obj"),
        ("P", "sInfHom", "(self : P) → sInfHom"),
        ("P", "εNFA", "(self : P) → εNFA"),
    ] {
        assert!(extract(&projection(parent, head, ty)).edges.contains(&edge(parent, head, EdgeRelation::Extends)), "universe/Unicode/lowercase literal head must be supported");
    }
}

#[test]
fn type_parser_does_not_invent_parents_from_prefixes_or_nested_binders() {
    for ty in [
        "(self : P → Q) → H",
        "(self : P × Q) → H",
        "(self : P) → H × Q",
        "∀ (_ : (self : P) → Nat), H",
        "((_ : (self : P) → Nat)) → H",
        "(self : P) → H + Q",
        "(self : P) → H.field",
        "(self : P) → ℝ",
        "(self : P) → (H",
        "(self : P) → H)",
        "(self : P) →",
        "(self : P) → → H",
        "(self : P) -> -> H",
    ] {
        let plan = extract(&projection("P", "H", ty));
        assert!(
            plan.edges
                .iter()
                .all(|intent| intent.relation != EdgeRelation::Extends),
            "unsupported or non-parent type must not qualify from a prefix: {ty}"
        );
        assert_eq!(
            plan.edges.len(),
            2,
            "type-text uncertainty must preserve both dependency intents"
        );
    }
}

#[test]
fn head_resolution_uses_references_and_actual_membership_without_guessing() {
    let rows = projection("Ns.P", "Ns.H", "(self : P) → H");
    assert!(extract(&rows)
        .edges
        .contains(&edge("Ns.P", "Ns.H", EdgeRelation::Extends)));
    let mut ambiguous = rows.clone();
    ambiguous.push(declaration("Other.H", "inductive", "Type", &[]));
    ambiguous[2].refs.push("Other.H".into());
    assert!(
        extract(&ambiguous)
            .edges
            .iter()
            .all(|e| e.relation != EdgeRelation::Extends),
        "two reference heads cannot be disambiguated by suffix or corpus order"
    );
    let mut missing = rows.clone();
    missing.remove(1);
    assert_eq!(
        extract(&missing).edges,
        vec![edge("Ns.P.toH", "Ns.P", EdgeRelation::DependsOn)],
        "refs_in_corpus naming does not establish actual membership"
    );
    let mut without_parent_ref = rows.clone();
    without_parent_ref[2].refs = vec!["Ns.H".into()];
    assert!(
        extract(&without_parent_ref)
            .edges
            .iter()
            .all(|e| e.relation != EdgeRelation::Extends),
        "self's abbreviated head must also resolve through supplied refs"
    );
    let mut exact = ambiguous;
    exact[2].type_expr = "(self : Ns.P) → Ns.H".into();
    assert!(extract(&exact)
        .edges
        .contains(&edge("Ns.P", "Ns.H", EdgeRelation::Extends)));
    let mut exact_and_suffix = projection("P", "H", "(self : P) → H");
    exact_and_suffix.push(declaration("Other.H", "inductive", "Type", &[]));
    exact_and_suffix[2].refs.push("Other.H".into());
    assert!(
        extract(&exact_and_suffix)
            .edges
            .iter()
            .all(|e| e.relation != EdgeRelation::Extends),
        "exact and qualified-suffix reference candidates are ambiguous together"
    );
    for keyword in ["fun", "let", "by", "_"] {
        let rows = projection("P", keyword, &format!("(self : P) → {keyword}"));
        assert!(
            extract(&rows)
                .edges
                .iter()
                .all(|e| e.relation != EdgeRelation::Extends),
            "a reserved head must not qualify even when named in refs: {keyword}"
        );
    }
}

#[test]
fn invalid_corpus_is_refused_and_forbidden_pairs_remain_explicit_intents() {
    let duplicate = vec![
        declaration("A", "def", "Nat", &[]),
        declaration("A", "theorem", "Prop", &[]),
    ];
    assert!(matches!(
        FormalExtractor.extract(&duplicate, &OntologySpec::formal_math().unwrap()),
        Err(ExtractError::DuplicateName {
            first_record: 1,
            duplicate_record: 2,
            ..
        })
    ));
    assert!(matches!(
        FormalExtractor.extract(
            &[declaration("", "def", "Nat", &[])],
            &OntologySpec::formal_math().unwrap()
        ),
        Err(ExtractError::EmptyName { record: 1 })
    ));
    assert!(matches!(
        FormalExtractor.extract(
            &[declaration("A", "made_up", "Nat", &[])],
            &OntologySpec::formal_math().unwrap()
        ),
        Err(ExtractError::Kind { .. })
    ));
    let forbidden = [
        declaration("Structure", "inductive", "Type", &["Proof"]),
        declaration("Proof", "theorem", "Prop", &[]),
    ];
    assert_eq!(
        extract(&forbidden).edges,
        vec![edge("Structure", "Proof", EdgeRelation::DependsOn)],
        "extractor must not erase an illegal endpoint pair before runtime can refuse it"
    );
}

#[test]
fn supplied_ontology_mapping_and_first_seen_edges_drive_extraction() {
    let formal = OntologySpec::formal_math().unwrap();
    let mut relations = formal.relations().clone();
    relations.reference = EdgeRelation::Extends;
    relations.parent_projection = EdgeRelation::DependsOn;
    let custom = OntologySpec::try_new(
        Some(formal.vocabulary().to_vec()),
        Some(relations),
        Some(*formal.taxonomy()),
    )
    .unwrap();
    let rows = projection("P", "H", "(self : P) → H");
    let plan = FormalExtractor.extract(&rows, &custom).unwrap();
    assert_eq!(
        plan.edges[0],
        LinkIntent {
            source: "P.toH".into(),
            target: "P".into(),
            relation: EdgeRelation::Extends,
            dependency_kind: None
        },
        "reference mapping comes from the supplied ontology, without runtime metadata on extends"
    );
    assert_eq!(
        plan.edges[2],
        LinkIntent {
            source: "P".into(),
            target: "H".into(),
            relation: EdgeRelation::DependsOn,
            dependency_kind: Some(DependencyKind::Runtime)
        },
        "projection mapping comes from the supplied ontology, with runtime metadata on depends_on"
    );
    let mut duplicate = rows;
    duplicate[0].refs = vec!["H".into(), "H".into()];
    let mut same_relation = formal.relations().clone();
    same_relation.parent_projection = EdgeRelation::DependsOn;
    let custom = OntologySpec::try_new(
        Some(formal.vocabulary().to_vec()),
        Some(same_relation),
        Some(*formal.taxonomy()),
    )
    .unwrap();
    let plan = FormalExtractor.extract(&duplicate, &custom).unwrap();
    let pair = plan
        .edges
        .iter()
        .filter(|intent| intent.source == "P" && intent.target == "H")
        .collect::<Vec<_>>();
    assert_eq!(
        pair.len(),
        1,
        "an identical complete edge intent must retain only its first occurrence"
    );
    assert_eq!(pair[0].dependency_kind, Some(DependencyKind::Runtime));
}
